//! The icon system's artwork (docs/design-icons.md): one set of two-tone
//! symbols, drawn on paper for files, pressed into folders for places, and
//! bare in the sidebar. This module only builds SVG text; `ui::icon`
//! rasterizes it once per key and caches the image.
//!
//! Shapes sit on a 32-unit grid with 2 units of clear space. Detail is
//! drawn per [`SizeClass`], not scaled: small icons drop what would turn to
//! mush, cards gain a label and a lit top edge.

use filex::file_types::FileFamily;
use filex::settings::FolderIcon;

use super::icon_palette::{FileRamp, FolderRamp, hex};

/// One symbol from `assets/symbols/` (12-unit grid; a `#secondary` layer
/// at 35% under a `#primary` layer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Symbol {
    Text,
    Pdf,
    Sheet,
    Slides,
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Font,
    App,
    Home,
    Desktop,
    Download,
    Drive,
    Clock,
    Star,
    Trash,
}

macro_rules! symbols {
    ($($variant:ident => $name:literal),* $(,)?) => {
        impl Symbol {
            #[cfg(test)]
            pub const ALL: &[Symbol] = &[$(Symbol::$variant),*];

            /// The asset path `svg().path()` loads (see `ui::assets`).
            pub fn path(self) -> &'static str {
                match self { $(Symbol::$variant => concat!("symbols/", $name, ".svg")),* }
            }

            /// The embedded SVG source.
            pub fn source(self) -> &'static str {
                match self {
                    $(Symbol::$variant => include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"), "/assets/symbols/", $name, ".svg"
                    ))),*
                }
            }
        }
    };
}

symbols! {
    Text => "text", Pdf => "pdf", Sheet => "sheet", Slides => "slides",
    Image => "image", Video => "video", Audio => "audio", Archive => "archive",
    Code => "code", Font => "font", App => "app", Home => "home",
    Desktop => "desktop", Download => "download", Drive => "drive",
    Clock => "clock", Star => "star", Trash => "trash",
}

impl Symbol {
    /// Symbols whose secondary layer is only a container (a page, a frame).
    /// Pressed into a folder, the folder is the container, so it's dropped.
    fn boxed(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Pdf | Self::Image | Self::Video | Self::Archive
        )
    }

    /// A family's symbol; `Other` has none (the paper shows plain lines).
    pub fn of_family(family: FileFamily) -> Option<Self> {
        Some(match family {
            FileFamily::Text => Self::Text,
            FileFamily::Pdf => Self::Pdf,
            FileFamily::Spreadsheet => Self::Sheet,
            FileFamily::Presentation => Self::Slides,
            FileFamily::Image => Self::Image,
            FileFamily::Video => Self::Video,
            FileFamily::Audio => Self::Audio,
            FileFamily::Archive => Self::Archive,
            FileFamily::Code => Self::Code,
            FileFamily::Font => Self::Font,
            FileFamily::App => Self::App,
            FileFamily::Other => return None,
        })
    }

    /// The symbol pressed into a special folder.
    pub fn of_folder(icon: FolderIcon) -> Option<Self> {
        Some(match icon {
            FolderIcon::Plain => return None,
            FolderIcon::Pictures => Self::Image,
            FolderIcon::Music => Self::Audio,
            FolderIcon::Videos => Self::Video,
            FolderIcon::Documents => Self::Text,
            FolderIcon::Downloads => Self::Download,
            FolderIcon::Code => Self::Code,
            FolderIcon::Archives => Self::Archive,
            FolderIcon::Desktop => Self::Desktop,
        })
    }

    /// The symbol's layers as SVG fragments in `color`, placed at (x, y)
    /// on the 32-unit grid and `size` units wide.
    fn place(self, x: f32, y: f32, size: f32, color: &str, secondary: bool) -> String {
        let source = self.source();
        let open = source.find('>').map_or(0, |i| i + 1);
        let close = source.rfind("</svg>").unwrap_or(source.len());
        let mut inner = source[open..close].to_string();
        if !secondary
            && let Some(start) = inner.find("<g id=\"secondary\"")
            && let Some(end) = inner[start..].find("</g>")
        {
            inner.replace_range(start..start + end + 4, "");
        }
        let inner = inner.replace("currentColor", color);
        format!(
            r#"<g transform="translate({x} {y}) scale({s})" fill="{color}">{inner}</g>"#,
            s = size / 12.
        )
    }
}

/// How much detail an icon carries at a given edge (px).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeClass {
    /// 16–19px (Compact rows): flat fills, no fold shading, primary only.
    Micro,
    /// 20–31px (Comfortable rows, sidebar, drag preview).
    List,
    /// 32px and up (grid, details, Quick Look): label, highlight, shadow.
    Card,
}

impl SizeClass {
    pub fn of(edge: f32) -> Self {
        if edge < 20. {
            Self::Micro
        } else if edge < 32. {
            Self::List
        } else {
            Self::Card
        }
    }
}

/// The paper, 22 × 28 units with a 3-unit radius and the fold cut away.
const PAGE: &str = "M8 2h11.5L27 9.5V27a3 3 0 0 1-3 3H8a3 3 0 0 1-3-3V5a3 3 0 0 1 3-3Z";
const FOLD: &str = "M19.5 2v5a2.5 2.5 0 0 0 2.5 2.5h5Z";
/// Where a card's extension label sits (x, y, width, height), in units.
/// `ui::icon` lays the label text over the same box.
pub const LABEL_BOX: (f32, f32, f32, f32) = (8., 21.6, 16., 5.4);

fn document(body: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 32 32"><defs><filter id="soft" x="-20%" y="-20%" width="140%" height="140%"><feGaussianBlur stdDeviation=".6"/></filter></defs>{body}</svg>"#
    )
}

/// Plain lines for a family with no symbol.
fn lines(ink: &str, rows: &[(f32, f32)], thickness: f32) -> String {
    rows.iter()
        .map(|&(y, width)| {
            format!(
                r#"<rect x="10" y="{y}" width="{width}" height="{thickness}" rx="{r}" fill="{ink}" fill-opacity=".45"/>"#,
                r = thickness / 2.
            )
        })
        .collect()
}

/// A file icon. `labelled` reserves the card's extension label (its text
/// is laid over by the caller, so one raster serves every extension).
pub fn file_svg(
    family: FileFamily,
    ramp: &FileRamp,
    class: SizeClass,
    dark: bool,
    labelled: bool,
) -> String {
    let ink = hex(ramp.ink);
    let symbol = Symbol::of_family(family);
    if class == SizeClass::Micro {
        let mark = match symbol {
            Some(s) => s.place(8.5, 12.5, 15., &ink, false),
            None => lines(&ink, &[(15., 12.), (20., 8.)], 2.2),
        };
        return document(&format!(
            r#"<path d="{PAGE}" fill="{fold}"/><path d="{FOLD}" fill="{ink}" fill-opacity=".35"/>{mark}"#,
            fold = hex(ramp.fold)
        ));
    }
    let card = class == SizeClass::Card;
    let (edge, edge_alpha) = if dark {
        ("#000000".to_string(), 0.55)
    } else {
        (hex(ramp.edge), 0.42)
    };
    let mut body = format!(
        r#"<defs><linearGradient id="paper" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient></defs>"#,
        hex(ramp.paper_top),
        hex(ramp.paper_bottom)
    );
    if card {
        body += &format!(
            r##"<path d="{PAGE}" transform="translate(0 .6)" fill="#000000" fill-opacity="{}" filter="url(#soft)"/>"##,
            if dark { 0.35 } else { 0.1 }
        );
    }
    body += &format!(
        r#"<path d="{PAGE}" fill="url(#paper)"/><path d="{PAGE}" fill="none" stroke="{edge}" stroke-opacity="{edge_alpha}" stroke-width=".75"/>"#
    );
    if card {
        body += r##"<path d="M8.2 2.75h10.9" stroke="#ffffff" stroke-opacity=".9" stroke-width=".75" stroke-linecap="round"/>"##;
    }
    body += &format!(
        r#"<path d="{FOLD}" fill="{fold}"/><path d="{FOLD}" fill="none" stroke="{edge}" stroke-opacity=".3" stroke-width=".5"/>"#,
        fold = hex(ramp.fold),
        edge = hex(ramp.edge)
    );
    body += &match (symbol, card) {
        (Some(s), false) => s.place(10., 13., 12., &ink, true),
        (Some(s), true) => s.place(11.25, 10.5, 9.5, &ink, true),
        (None, false) => lines(&ink, &[(15., 12.), (19., 12.), (23., 7.)], 1.6),
        (None, true) => lines(&ink, &[(12., 12.), (15.2, 9.)], 1.3),
    };
    if card && labelled {
        let (x, y, w, h) = LABEL_BOX;
        body += &format!(
            r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="1.6" fill="{}"/>"#,
            hex(ramp.label)
        );
    }
    document(&body)
}

/// The folder's back (with its tab) and front panel, 26 × 22.5 units.
const FOLDER_BACK: &str = "M3 8.2A2.7 2.7 0 0 1 5.7 5.5h5.6c1 0 1.7.4 2.3 1.1l.9 1c.5.6 1.1.9 1.9.9h9.9A2.7 2.7 0 0 1 29 11.2v14.1a2.7 2.7 0 0 1-2.7 2.7H5.7A2.7 2.7 0 0 1 3 25.3Z";
const FOLDER_FRONT: &str = "M3 15.3a2.7 2.7 0 0 1 2.7-2.7h20.6a2.7 2.7 0 0 1 2.7 2.7v10a2.7 2.7 0 0 1-2.7 2.7H5.7A2.7 2.7 0 0 1 3 25.3Z";

/// A folder in the accent's ramp, with an optional pressed-in symbol.
pub fn folder_svg(
    ramp: &FolderRamp,
    symbol: Option<Symbol>,
    class: SizeClass,
    dark: bool,
) -> String {
    let card = class == SizeClass::Card;
    let micro = class == SizeClass::Micro;
    let mut body = format!(
        r#"<defs><linearGradient id="back" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient><linearGradient id="front" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient></defs>"#,
        hex(ramp.back_top),
        hex(ramp.back_bottom),
        hex(ramp.front_top),
        hex(ramp.front_bottom)
    );
    if card {
        body += &format!(
            r##"<path d="{FOLDER_FRONT}" transform="translate(0 .7)" fill="#000000" fill-opacity="{}" filter="url(#soft)"/>"##,
            if dark { 0.4 } else { 0.12 }
        );
    }
    body += &format!(r#"<path d="{FOLDER_BACK}" fill="url(#back)"/>"#);
    if card {
        body += &format!(
            r##"<rect x="5.6" y="9.4" width="20.8" height="6" rx="1.2" fill="{}"/><rect x="6.8" y="10.5" width="18.4" height="5" rx="1" fill="#ffffff"/>"##,
            hex(ramp.sheet)
        );
    }
    if !micro {
        // The front panel casts a soft shadow on what's behind it.
        body += &format!(
            r##"<rect x="3" y="11.9" width="26" height="1.6" rx=".8" fill="#000000" fill-opacity="{}" filter="url(#soft)"/>"##,
            if dark { 0.3 } else { 0.16 }
        );
    }
    body += &format!(r#"<path d="{FOLDER_FRONT}" fill="url(#front)"/>"#);
    if !micro {
        body += &format!(
            r#"<path d="M5.4 13.3h21.2" stroke="{}" stroke-width=".8" stroke-linecap="round"/>"#,
            hex(ramp.lip)
        );
    }
    let (back_alpha, front_alpha) = if dark { (0.6, 0.5) } else { (0.22, 0.18) };
    body += &format!(
        r#"<path d="{FOLDER_BACK}" fill="none" stroke="{edge}" stroke-opacity="{back_alpha}" stroke-width=".5"/><path d="{FOLDER_FRONT}" fill="none" stroke="{edge}" stroke-opacity="{front_alpha}" stroke-width=".5"/>"#,
        edge = hex(ramp.edge)
    );
    if let Some(symbol) = symbol.filter(|_| !micro) {
        // Without its frame a boxed symbol has spare margin, so it's larger.
        let boxed = symbol.boxed();
        let size = if card { 8.6 } else { 10.5 } * if boxed { 1.3 } else { 1. };
        let (x, y) = (16. - size / 2., 20.4 - size / 2.);
        // Pressed in: a white copy below catches the light, the mark on top.
        body += &format!(
            r#"<g opacity=".55">{}</g><g opacity=".82">{}</g>"#,
            symbol.place(x, y + 0.55, size, "#ffffff", !boxed),
            symbol.place(x, y, size, &hex(ramp.mark), !boxed)
        );
    }
    document(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::icon_palette::{file_ramp, folder_ramp};

    fn parses(svg: &str) {
        resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())
            .unwrap_or_else(|err| panic!("{err}: {svg}"));
    }

    #[test]
    fn every_symbol_has_both_layers() {
        for symbol in Symbol::ALL {
            let source = symbol.source();
            assert!(source.contains(r#"<g id="secondary""#), "{symbol:?}");
            assert!(source.contains(r#"<g id="primary""#), "{symbol:?}");
            parses(source);
        }
    }

    #[test]
    fn dropping_the_secondary_layer_keeps_the_primary() {
        let with = Symbol::Image.place(0., 0., 12., "#123456", true);
        let without = Symbol::Image.place(0., 0., 12., "#123456", false);
        assert!(with.contains("secondary") && !without.contains("secondary"));
        assert!(without.contains("primary") && !without.contains("currentColor"));
    }

    #[test]
    fn every_file_icon_parses_at_every_size() {
        for dark in [false, true] {
            for family in FileFamily::ALL {
                let ramp = file_ramp(family, dark);
                for class in [SizeClass::Micro, SizeClass::List, SizeClass::Card] {
                    parses(&file_svg(family, &ramp, class, dark, true));
                }
            }
        }
    }

    #[test]
    fn every_folder_parses_at_every_size() {
        let ramp = folder_ramp(gpui::rgb(0x087eaf), false);
        for icon in [
            FolderIcon::Plain,
            FolderIcon::Pictures,
            FolderIcon::Music,
            FolderIcon::Videos,
            FolderIcon::Documents,
            FolderIcon::Downloads,
            FolderIcon::Code,
            FolderIcon::Archives,
            FolderIcon::Desktop,
        ] {
            for class in [SizeClass::Micro, SizeClass::List, SizeClass::Card] {
                parses(&folder_svg(&ramp, Symbol::of_folder(icon), class, false));
            }
        }
    }

    #[test]
    fn size_classes_follow_the_edge() {
        assert_eq!(SizeClass::of(16.), SizeClass::Micro);
        assert_eq!(SizeClass::of(20.), SizeClass::List);
        assert_eq!(SizeClass::of(31.), SizeClass::List);
        assert_eq!(SizeClass::of(32.), SizeClass::Card);
        assert_eq!(SizeClass::of(192.), SizeClass::Card);
    }

    /// Micro icons are flat: no gradients, highlight or label at 16px.
    #[test]
    fn micro_icons_drop_fine_detail() {
        let ramp = file_ramp(FileFamily::Pdf, false);
        let micro = file_svg(FileFamily::Pdf, &ramp, SizeClass::Micro, false, true);
        assert!(!micro.contains("linearGradient") && !micro.contains("rx=\"1.6\""));
        let card = file_svg(FileFamily::Pdf, &ramp, SizeClass::Card, false, true);
        assert!(card.contains("rx=\"1.6\""));
    }
}
