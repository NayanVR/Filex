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
    search::filter::ItemMeta,
    search::literal::Tier,
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
        .any(|f| matches!(f, crate::search::filter::Filter::Size(_)));
    let need_mtime = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search::filter::Filter::Modified(_)));
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
            .estimate(needle.as_bytes())
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
    emit: impl FnMut(Batch) -> Result<()>,
) -> Result<()> {
    ensure!(
        query.epoch_hint.is_none_or(|e| e == view.epoch),
        "search epoch expired"
    );
    ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
    let need_size = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search::filter::Filter::Size(_)));
    let need_mtime = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search::filter::Filter::Modified(_)));
    // Small scopes enumerate a complete live-view set; a capped scope is
    // never treated as complete. Tag paths remain an exact final predicate:
    // resolving one ID per path could hide transient duplicate paths in overlays.
    const SCOPE_BOUND: usize = 16_384;
    let selected = query.scope.as_ref().and_then(|scope| {
        let root = view.resolve(scope)?;
        let mut ids = view.scoped_ids(scope, SCOPE_BOUND, cancel);
        if ids.len() >= SCOPE_BOUND {
            return None;
        }
        ids.push(root);
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    });
    ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
    if let Some(ids) = &selected {
        return stream_records(
            view,
            query,
            cancel,
            ids.iter()
                .filter_map(|id| view.record_projected(*id, need_size, need_mtime)),
            emit,
        );
    }
    let needle = nfc_fold(&query.text);
    let mut candidates = view.base.exhaustive_candidates(&query.filters, cancel)?;
    // Tiny metadata sets are cheaper to verify directly. Signature estimates
    // can be very loose: don't let them send hundreds of thousands of metadata
    // candidates through native-name decoding instead of dictionary lookup.
    // A ubiquitous single byte without metadata can stream immediately; eagerly
    // materializing its huge posting set delays the preview's early-stop cap.
    let direct = candidates.as_ref().is_some_and(|set| set.count() <= 4096)
        || (candidates.is_none()
            && needle.len() == 1
            && view.base.search.estimate(needle.as_bytes())
                >= view.base.search.name_count().div_ceil(2));
    if !needle.is_empty() && !direct {
        let text = view
            .base
            .search
            .exhaustive_candidates(&needle, view.base.len(), cancel)?;
        if let Some(set) = &mut candidates {
            set.intersect(&text);
        } else {
            candidates = Some(text);
        }
    }
    ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
    // Base postings describe the immutable snapshot only. Every changed ID is
    // evaluated from its newest overlay, including records that newly match.
    let mut changes = std::collections::BTreeMap::new();
    for layer in &view.layers {
        for (&id, record) in &layer.records {
            ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
            changes.insert(id, record.as_ref());
        }
    }
    let slots: Box<dyn Iterator<Item = usize> + '_> = match &candidates {
        Some(set) => Box::new(set.iter()),
        None => Box::new(0..view.base.len()),
    };
    // Merge by stable FileId, and reject numeric bucket edges before touching
    // compressed native names. Paths are resolved only for matching records.
    let mut base = slots
        .take_while(|_| !cancel.load(Ordering::Relaxed))
        .filter_map(|slot| {
            let id = view.base.id(slot);
            (!changes.contains_key(&id) && view.base.matches_numeric(slot, &query.filters))
                .then_some((id, slot))
        })
        .peekable();
    let mut overlays = changes
        .iter()
        .filter_map(|(&id, record)| record.map(|r| (id, r)))
        .peekable();
    let records = std::iter::from_fn(|| {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if overlays
            .peek()
            .is_some_and(|(id, _)| base.peek().is_none_or(|(next, _)| id < next))
        {
            return overlays.next().map(|(_, record)| record.clone());
        }
        base.next()
            .map(|(_, slot)| view.base.record_projected(slot, need_size, need_mtime))
    });
    stream_records(view, query, cancel, records, emit)
}

fn stream_records(
    view: &View,
    query: &Query,
    cancel: &AtomicBool,
    records: impl Iterator<Item = Record>,
    mut emit: impl FnMut(Batch) -> Result<()>,
) -> Result<()> {
    let allowed = query
        .allowed
        .as_ref()
        .map(|paths| paths.iter().map(|p| p.as_path()).collect::<HashSet<_>>());
    let needle = nfc_fold(&query.text);
    let mut batch = Vec::with_capacity(256);
    let (mut scanned, mut total) = (0u64, 0u64);
    for record in records {
        ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
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
    ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
    emit(Batch {
        epoch: view.epoch,
        hits: batch,
        scanned,
        total: Some(total),
    })
}

/// Local benchmark baseline retained to measure Magic separately from ranked search.
#[cfg(feature = "index-v2-lab")]
pub fn stream_before_indexed_magic(
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
        .any(|f| matches!(f, crate::search::filter::Filter::Size(_)));
    let need_mtime = query
        .filters
        .iter()
        .any(|f| matches!(f, crate::search::filter::Filter::Modified(_)));
    let needle = nfc_fold(&query.text);
    let mut batch = Vec::with_capacity(256);
    let (mut scanned, mut total) = (0u64, 0u64);
    // Small folder scopes use the catalog's child ranges. If enumeration hits
    // the bound, fall back to the full stream: a preview must never silently
    // truncate a plan. Include the scope folder itself, matching full-scan rules.
    const SCOPE_BOUND: usize = 16_384;
    let scoped = query.scope.as_ref().and_then(|scope| {
        let root = view.resolve(scope)?;
        let mut ids = view.scoped_ids(scope, SCOPE_BOUND, cancel);
        if ids.len() >= SCOPE_BOUND {
            return None;
        }
        ids.push(root);
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    });
    // Rare literal queries can enumerate every posting directly. A bounded
    // candidate set is used only when complete; common/empty queries still
    // stream the catalog. Overlay IDs are included so renames are never lost.
    let scoped = scoped.or_else(|| {
        if needle.is_empty() {
            return None;
        }
        let names = view.base.search.substring(&needle, SCOPE_BOUND);
        if names.len() >= SCOPE_BOUND {
            return None;
        }
        let mut ids = Vec::new();
        for name in names {
            for slot in view.base.search.files(name) {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                ids.push(view.base.id(slot as usize));
                if ids.len() >= SCOPE_BOUND {
                    return None;
                }
            }
        }
        for layer in &view.layers {
            for id in layer.records.keys() {
                ids.push(*id);
                if ids.len() >= SCOPE_BOUND {
                    return None;
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    });
    let records: Box<dyn Iterator<Item = Record> + '_> = match &scoped {
        Some(ids) => Box::new(
            ids.iter()
                .filter_map(|id| view.record_projected(*id, need_size, need_mtime)),
        ),
        None => Box::new(view.records_projected(need_size, need_mtime)),
    };
    // Ordered by stable FileId; fuzzy never enters an exhaustive operation.
    for record in records {
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
