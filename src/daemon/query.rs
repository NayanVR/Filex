//! Interactive bounded retrieval and a separate stable exhaustive stream.
use super::{
    ipc::{Batch, Hit, Page, Query},
    view::View,
};
use crate::{
    catalog::{
        normalize::{boundaries, nfc_fold},
        segment::Record,
    },
    search::literal::Tier,
    search_filter::ItemMeta,
};
use anyhow::{Result, ensure};
use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
const WORK_BUDGET: usize = 20_000;
pub fn literal_tier(raw: &[u8], needle: &str) -> Option<Tier> {
    if needle.is_empty() {
        return Some(Tier::Substring);
    }
    let raw = std::str::from_utf8(raw).ok()?;
    let name = nfc_fold(raw);
    if name == needle {
        Some(Tier::Exact)
    } else if name.starts_with(needle) {
        Some(Tier::Prefix)
    } else if name.contains(needle) {
        let boundary = boundaries(raw)
            .into_iter()
            .any(|p| nfc_fold(&raw[p..]).starts_with(needle));
        Some(if boundary {
            Tier::Boundary
        } else {
            Tier::Substring
        })
    } else {
        None
    }
}
fn hit(
    view: &View,
    record: Record,
    tier: Tier,
    query: &Query,
    allowed: &Option<HashSet<&Path>>,
) -> Option<Hit> {
    if record.parent == 0 {
        return None;
    }
    let name = crate::catalog::segment::os_name(&record.name)
        .to_string_lossy()
        .into_owned();
    if !query.filters.iter().all(|f| {
        f.matches(&ItemMeta {
            name: &name,
            is_dir: record.is_dir(),
            size: record.size,
            mtime: record.mtime,
        })
    }) {
        return None;
    }
    let path = view.path(record.id)?;
    if query
        .scope
        .as_ref()
        .is_some_and(|scope| !path.starts_with(scope))
    {
        return None;
    }
    if allowed
        .as_ref()
        .is_some_and(|set| !set.contains(path.as_path()))
    {
        return None;
    }
    Some(Hit {
        id: record.id,
        name,
        path,
        is_dir: record.is_dir(),
        identity: record.identity,
        tier,
    })
}
pub fn verify(hit: &Hit) -> Result<()> {
    let meta = std::fs::symlink_metadata(&hit.path)?;
    let current = crate::ingest::identity(&hit.path, &meta);
    ensure!(
        current.key != 0 && hit.identity.key != 0,
        "native identity unavailable; browse to the file before opening or changing it"
    );
    ensure!(
        current == hit.identity,
        "filesystem identity changed; search again"
    );
    Ok(())
}
pub fn search(
    view: &View,
    query: &Query,
    cancel: &AtomicBool,
    hot: &HashMap<u64, u64>,
) -> Result<Page> {
    ensure!(
        query.limit <= 1000 && query.offset <= 100_000 && query.text.len() <= 4096,
        "query exceeds interactive bounds"
    );
    ensure!(
        query.epoch_hint.is_none_or(|e| e == view.epoch),
        "search epoch expired"
    );
    let allowed = query
        .allowed
        .as_ref()
        .map(|paths| paths.iter().map(|p| p.as_path()).collect::<HashSet<_>>());
    let need_size = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search_filter::Filter::Size(_)));
    let need_mtime = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search_filter::Filter::Modified(_)));
    let needle = nfc_fold(&query.text);
    let limit = query.limit + query.offset + 1;
    let changed_ids: HashSet<u64> = view
        .layers
        .iter()
        .flat_map(|layer| layer.records.keys().copied())
        .collect();
    let mut hits = HashMap::<u64, Hit>::new();
    let mut examined = 0;
    let mut capped = false;
    let mut partial = false;
    let consider = |id: u64,
                    tier: Tier,
                    hits: &mut HashMap<u64, Hit>,
                    examined: &mut usize,
                    capped: &mut bool| {
        if cancel.load(Ordering::Relaxed) || *examined >= WORK_BUDGET {
            *capped = true;
            return false;
        }
        *examined += 1;
        if let Some(record) = view.record_projected(id, need_size, need_mtime) {
            if let Some(actual) = literal_tier(&record.name, &needle) {
                if let Some(hit) = hit(view, record, actual, query, &allowed) {
                    hits.insert(id, hit);
                }
            } else if tier == Tier::Fuzzy
                && let Some(hit) = hit(view, record, tier, query, &allowed)
            {
                hits.insert(id, hit);
            }
        }
        true
    };
    let text_estimate = if needle.is_empty() {
        usize::MAX
    } else {
        view.base
            .search
            .range(needle.as_bytes())
            .len()
            .saturating_mul(3)
    };
    if let Some(paths) = query
        .allowed
        .as_ref()
        .filter(|p| p.len() <= WORK_BUDGET && p.len() <= text_estimate)
    {
        for path in paths.iter().take(WORK_BUDGET + 1) {
            if let Some(id) = view.resolve(path)
                && !consider(id, Tier::Substring, &mut hits, &mut examined, &mut capped)
            {
                break;
            }
        }
    } else {
        let scope = query
            .scope
            .as_ref()
            .filter(|_| text_estimate > 256)
            .map(|p| view.scoped_ids(p, WORK_BUDGET + 1, cancel));
        let filtered = view.base.filter_candidates(&query.filters, WORK_BUDGET + 1);
        if let Some(ids) = scope.filter(|ids| ids.len() <= WORK_BUDGET) {
            for id in ids {
                if !consider(id, Tier::Substring, &mut hits, &mut examined, &mut capped) {
                    break;
                }
            }
        } else if let Some(files) = filtered.filter(|f| {
            crate::search::planner::Plan::new(
                text_estimate,
                Some(f.len()),
                query.limit,
                WORK_BUDGET,
            )
            .order
                == crate::search::planner::Order::FilterFirst
        }) {
            for slot in files {
                if !consider(
                    view.base.id(slot as usize),
                    Tier::Substring,
                    &mut hits,
                    &mut examined,
                    &mut capped,
                ) {
                    break;
                }
            }
        } else {
            let mut count = (limit * 2).clamp(100, WORK_BUDGET);
            let mut seen = HashSet::new();
            loop {
                let names = if needle.is_empty() {
                    (0..view.base.search.name_count().min(count) as u32)
                        .map(|n| (Tier::Substring, n))
                        .collect::<Vec<_>>()
                } else {
                    view.base.search.search(&needle, count)
                };
                let complete = names.len() < count;
                let mut satisfied = false;
                'names: for (tier, name) in names {
                    for slot in view.base.search.files(name) {
                        if !seen.insert(slot) {
                            continue;
                        }
                        // An updated name has a different static rank. Its
                        // overlay entry is ranked separately below.
                        if changed_ids.contains(&view.base.id(slot as usize)) {
                            examined += 1;
                            if examined >= WORK_BUDGET {
                                break 'names;
                            }
                            continue;
                        }
                        if !consider(
                            view.base.id(slot as usize),
                            tier,
                            &mut hits,
                            &mut examined,
                            &mut capped,
                        ) {
                            break 'names;
                        }
                        // Names and postings arrive in static rank order. Case-variant
                        // boundary unions can downgrade a hit; count only hits at or
                        // above this tier before proving the unseen suffix is worse.
                        if hits.len() >= limit
                            && hits.values().filter(|h| h.tier <= tier).count() >= limit
                        {
                            satisfied = true;
                            break 'names;
                        }
                    }
                }
                if complete || count == WORK_BUDGET || examined >= WORK_BUDGET || satisfied {
                    partial |= !satisfied && !complete;
                    capped |= satisfied || !complete;
                    break;
                }
                count = (count * 2).min(WORK_BUDGET);
            }
        }
    }
    // Keep only the best page prefix while scanning the bounded overlay.
    // Expensive path materialization happens only for a candidate that can win.
    type RankKey = (Tier, std::cmp::Reverse<u64>, usize, String, u64);
    let key = |tier, name: &str, id| {
        let folded = nfc_fold(name);
        (
            tier,
            std::cmp::Reverse(hot.get(&id).copied().unwrap_or(0)),
            folded.len(),
            folded,
            id,
        )
    };
    let mut best: BinaryHeap<RankKey> = hits.values().map(|h| key(h.tier, &h.name, h.id)).collect();
    while best.len() > limit {
        let old = best.pop().unwrap();
        hits.remove(&old.4);
        capped = true;
    }
    let mut overlay_ids = changed_ids;
    overlay_ids.extend(hot.keys().copied());
    for id in overlay_ids {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if hits.contains_key(&id) {
            continue;
        }
        if let Some(record) = view.record_projected(id, need_size, need_mtime)
            && let Some(tier) = literal_tier(&record.name, &needle)
        {
            let name = crate::catalog::segment::os_name(&record.name)
                .to_string_lossy()
                .into_owned();
            let rank = key(tier, &name, id);
            if best.len() >= limit && best.peek().is_some_and(|worst| &rank >= worst) {
                continue;
            }
            if let Some(hit) = hit(view, record, tier, query, &allowed) {
                hits.insert(id, hit);
                best.push(rank);
                if best.len() > limit {
                    let old = best.pop().unwrap();
                    hits.remove(&old.4);
                    capped = true;
                }
            }
        }
    }
    if query.fuzzy && hits.len() < 10 && !needle.is_empty() {
        for name in view.base.search.fuzzy_names(&needle) {
            for slot in view.base.search.files(name).take(32) {
                if hits.len() >= 100 {
                    break;
                }
                let id = view.base.id(slot as usize);
                if let Some(record) = view.record_projected(id, need_size, need_mtime) {
                    if nfc_fold(&String::from_utf8_lossy(&record.name))
                        != view.base.search.name(name)
                    {
                        continue;
                    }
                    if let Some(hit) = hit(view, record, Tier::Fuzzy, query, &allowed) {
                        hits.entry(id).or_insert(hit);
                    }
                }
            }
        }
    }
    ensure!(!cancel.load(Ordering::Relaxed), "search cancelled");
    let mut hits: Vec<_> = hits.into_values().collect();
    hits.sort_by_cached_key(|h| {
        let folded = nfc_fold(&h.name);
        (
            h.tier,
            std::cmp::Reverse(hot.get(&h.id).copied().unwrap_or(0)),
            folded.len(),
            folded,
            h.id,
        )
    });
    let more = (capped && hits.len() > query.offset) || hits.len() > query.limit + query.offset;
    Ok(Page {
        partial: partial || examined >= WORK_BUDGET,
        epoch: view.epoch,
        hits: hits
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect(),
        more,
        examined,
    })
}
pub fn stream(
    view: &View,
    query: &Query,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Batch) -> Result<()>,
) -> Result<()> {
    ensure!(
        query.epoch_hint.is_none_or(|e| e == view.epoch),
        "search epoch expired"
    );
    let allowed = query
        .allowed
        .as_ref()
        .map(|paths| paths.iter().map(|p| p.as_path()).collect::<HashSet<_>>());
    let need_size = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search_filter::Filter::Size(_)));
    let need_mtime = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search_filter::Filter::Modified(_)));
    let needle = nfc_fold(&query.text);
    let mut batch = Vec::with_capacity(256);
    let (mut scanned, mut total) = (0u64, 0u64);
    // Ordered by stable FileId; fuzzy never enters an exhaustive operation.
    for record in view.records_projected(need_size, need_mtime) {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("stream cancelled");
        }
        scanned += 1;
        if let Some(tier) = literal_tier(&record.name, &needle)
            && let Some(hit) = hit(view, record, tier, query, &allowed)
        {
            batch.push(hit);
            total += 1;
        }
        if batch.len() == 256 || scanned % 4096 == 0 {
            emit(Batch {
                epoch: view.epoch,
                hits: std::mem::take(&mut batch),
                scanned,
                total: None,
            })?;
            std::thread::yield_now();
        }
    }
    emit(Batch {
        epoch: view.epoch,
        hits: batch,
        scanned,
        total: Some(total),
    })
}
