//! Live renderer workload using Filex's production cards, names, icons,
//! details pane and bounded thumbnail cache. Deliberately excludes indexing,
//! filesystem watchers and workspace chrome; this is a component benchmark.
use super::{decode, memory, thumbnails, ui, write_json};
use anyhow::Result;
use gpui::{prelude::*, *};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Instant};

struct Grid {
    backend: String,
    #[cfg(windows)]
    shell_workers: Option<super::native::ThumbnailWorkers>,
    paths: Vec<PathBuf>,
    output: PathBuf,
    scroll: UniformListScrollHandle,
    cache: thumbnails::Cache,
    started: Instant,
    previous_frame: Option<Instant>,
    frames: Vec<Value>,
    loads: Vec<Value>,
    initial_memory: Value,
    peak_jobs: usize,
    jobs: usize,
    clearing: bool,
    screenshot: Option<String>,
}
impl Grid {
    fn image(&mut self, index: usize, edge: f32, cx: &mut Context<Self>) -> AnyElement {
        let path = self.paths[index].clone();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if self.backend != "icons" && !self.clearing {
            if let Some(thumbnails::ThumbnailState::Ready(image)) = self.cache.get(&path) {
                return ui::icon::thumbnail_icon(image.clone(), edge);
            }
            if self.cache.start(&path) {
                self.jobs += 1;
                self.peak_jobs = self.peak_jobs.max(self.jobs);
                let backend = self.backend.clone();
                let start = Instant::now();
                #[cfg(windows)]
                let shell_result = self
                    .shell_workers
                    .as_ref()
                    .map(|pool| pool.request(path.clone()));
                let task = cx.background_executor().spawn(async move {
                    #[cfg(windows)]
                    if let Some(result) = shell_result {
                        let result = result
                            .await
                            .map_err(|_| {
                                anyhow::anyhow!("Shell thumbnail worker dropped its response")
                            })
                            .and_then(|result| result)
                            .map(|pixels| {
                                std::sync::Arc::new(RenderImage::new(vec![image::Frame::new(
                                    pixels,
                                )]))
                            });
                        return (path, result);
                    }
                    let result = decode(&backend, &path, false);
                    (path, result)
                });
                cx.spawn(async move |view,cx| {
                    let (path,result)=task.await;
                    let _=view.update(cx,|view,cx| {
                        view.jobs=view.jobs.saturating_sub(1);
                        view.loads.push(json!({"elapsed_s":view.started.elapsed().as_secs_f64(),
                            "request_to_ready_ms":start.elapsed().as_secs_f64()*1000.,"ok":result.is_ok(),
                            "error":result.as_ref().err().map(|e|format!("{e:#}"))}));
                        view.cache.finish(&path,result);cx.notify();
                    });
                }).detach();
            }
        }
        ui::icon::file_icon(
            &ui::theme::Theme::dark(),
            filex::listing::FileKind::of(&name, false),
            &name,
            edge,
        )
    }
}
impl Render for Grid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_started = Instant::now();
        let elapsed = self.started.elapsed().as_secs_f64();
        let theme = ui::theme::Theme::dark();
        let edge = ui::grid::card_size(1);
        let width = (f32::from(window.viewport_size().width) - 300. - 16.).max(160.);
        let cols = ui::grid::columns_for(width, ui::grid::cell_width(edge));
        let rows = self.paths.len().div_ceil(cols);
        let phase = if elapsed < 2. {
            "startup"
        } else if elapsed < 10. {
            "slow"
        } else if elapsed < 18. {
            "fast"
        } else if elapsed < 26. {
            "revisit"
        } else {
            "cleanup"
        };
        let row = match phase {
            "slow" => ((elapsed - 2.) * 5.) as usize,
            "fast" => ((elapsed - 10.) * 100.) as usize % rows,
            "revisit" => 39usize.saturating_sub(((elapsed - 18.) * 5.) as usize),
            _ => 0,
        }
        .min(rows.saturating_sub(1));
        self.scroll.scroll_to_item_strict(row, ScrollStrategy::Top);
        if phase == "cleanup" && !self.clearing {
            self.clearing = true;
            self.cache = thumbnails::Cache::default();
        }
        let entity = cx.entity().downgrade();
        window.on_next_frame(move |_window,cx| {
            let _=entity.update(cx,|view,cx| {
                let now=Instant::now();
                let interval=view.previous_frame.replace(now).map(|old|now.duration_since(old).as_secs_f64()*1000.);
                view.frames.push(json!({"phase":phase,"elapsed_s":elapsed,"frame_interval_ms":interval,
                    "render_to_callback_ms":now.duration_since(render_started).as_secs_f64()*1000.,"row":row,
                    "in_flight":view.jobs,"memory":if view.frames.len()%60==0 {memory()}else{Value::Null}}));
                if elapsed>8. && view.screenshot.is_none() {
                    #[cfg(windows)]
                    { view.screenshot=Some(super::native::capture_grid(&view.output.with_extension("png")).map(|_|"saved".to_owned()).unwrap_or_else(|e|e.to_string())); }
                    view.previous_frame=None; // Screenshot I/O is not a scrolling frame sample.
                    #[cfg(not(windows))]
                    {view.screenshot=Some("Windows capture only".into());}
                }
                if elapsed>=29. {
                    let success=view.jobs==0 && view.frames.len()>120 && (view.backend=="icons" || view.loads.iter().any(|load|load["ok"]==true));
                    let result=json!({"status":if success {"ok"}else{"partial"},"backend":view.backend,"scope":"real GPUI production components; not whole Workspace or physical display FPS",
                        "frames":view.frames,"loads":view.loads,"peak_in_flight":view.peak_jobs,"pending_at_exit":view.jobs,
                        "memory_before":view.initial_memory,"memory_after_cleanup":memory(),"screenshot":view.screenshot,
                        "gpu":_window.gpu_specs().map(|gpu|json!({"device":gpu.device_name,"driver":gpu.driver_name,"driver_info":gpu.driver_info,"software_emulated":gpu.is_software_emulated})),
                        "items":view.paths.len(),"cache_capacity":thumbnails::CACHE_CAP,"full_file_viewer":"not implemented on Windows"});
                    if let Err(error)=write_json(&view.output,&result) {eprintln!("{error:#}");std::process::exit(1);}
                    cx.quit();
                } else {cx.notify();}
            });
        });
        let selected = (row * cols).min(self.paths.len() - 1);
        let preview = self.image(selected, 180., cx);
        let name = self.paths[selected]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let column_width = ui::grid::column_width(width, cols);
        let list = uniform_list(
            "benchmark-grid",
            rows,
            cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|row| {
                        let mut strip = ui::grid::grid_row(edge);
                        for index in row * cols..((row + 1) * cols).min(view.paths.len()) {
                            let name = view.paths[index]
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned();
                            let image = view.image(index, edge, cx);
                            strip =
                                strip.child(
                                    ui::grid::card(
                                        &theme,
                                        ElementId::Integer(index as u64),
                                        edge,
                                        column_width,
                                        false,
                                    )
                                    .child(ui::grid::card_icon_area(edge).child(image))
                                    .child(ui::grid::card_name(&theme, column_width, &name, cx)),
                                );
                        }
                        strip
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .size_full()
            .bg(theme.bg)
            .flex()
            .child(div().flex_1().h_full().child(list))
            .child(
                ui::details::panel(&theme, 300.)
                    .child(ui::details::preview_box(&theme).child(preview))
                    .child(ui::details::title(&theme, name))
                    .child(ui::details::meta_row(
                        &theme,
                        "Workload",
                        format!("{} / {phase}", self.backend),
                    )),
            )
    }
}
pub fn run(backend: String, dir: PathBuf, output: PathBuf) -> Result<()> {
    let mut paths = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    anyhow::ensure!(!paths.is_empty(), "empty scroll corpus");
    let initial_memory = memory();
    #[cfg(windows)]
    let shell_workers = if backend == "shell" {
        Some(super::native::ThumbnailWorkers::new(
            thumbnails::MAX_IN_FLIGHT,
        )?)
    } else {
        None
    };
    gpui_platform::application()
        .with_assets(ui::assets::Assets)
        .run(move |cx| {
            ui::fonts::register(cx);
            cx.set_global(ui::theme::Theme::dark());
            let bounds = Bounds::centered(None, size(px(1120.), px(760.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Filex scrolling benchmark".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                move |_window, cx| {
                    cx.activate(true);

                    cx.new(|_| Grid {
                        backend,
                        #[cfg(windows)]
                        shell_workers,
                        paths,
                        output,
                        scroll: UniformListScrollHandle::new(),
                        cache: thumbnails::Cache::default(),
                        started: Instant::now(),
                        previous_frame: None,
                        frames: Vec::new(),
                        loads: Vec::new(),
                        initial_memory,
                        peak_jobs: 0,
                        jobs: 0,
                        clearing: false,
                        screenshot: None,
                    })
                },
            )
            .expect("benchmark GPU window creation failed");
        });
    Ok(())
}
