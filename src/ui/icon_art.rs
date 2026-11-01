//! The icon system's artwork (docs/design-icons.md): one set of two-tone
//! symbols (Phosphor Duotone), drawn on paper for files and bare in the
//! sidebar; special folders press in the same icon's Phosphor Regular
//! outline. This module only builds SVG text; `ui::icon` rasterizes it
//! once per key and caches the image.
//!
//! Shapes sit on a 32-unit grid with 2 units of clear space. Detail is
//! drawn per [`SizeClass`], not scaled: small icons drop what would turn to
//! mush, cards gain a label and a lit top edge.

use filex::file_types::FileFamily;
use filex::settings::FolderIcon;

use super::icon_palette::{FileRamp, FolderRamp, hex};

/// Phosphor's drawing grid: every symbol and mark is 256 units square,
/// with its own padding built in.
const SYMBOL_GRID: f32 = 256.;

/// One symbol from `assets/symbols/` (Phosphor Duotone on its 256-unit
/// grid: a `#secondary` interior at 20% under a `#primary` outline).
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

/// One Regular (outline) mark from `assets/symbols/marks/`.
macro_rules! mark {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/symbols/marks/",
            $name,
            ".svg"
        ))
    };
}

impl Symbol {
    /// The Phosphor Regular outline pressed into a special folder. Crisper
    /// than the duotone on the folder's gradient. `None` for symbols no
    /// folder carries.
    fn mark_source(self) -> Option<&'static str> {
        Some(match self {
            Self::Image => mark!("image"),
            Self::Audio => mark!("audio"),
            Self::Video => mark!("video"),
            Self::Text => mark!("text"),
            Self::Download => mark!("download"),
            Self::Code => mark!("code"),
            Self::Archive => mark!("archive"),
            Self::Desktop => mark!("desktop"),
            _ => return None,
        })
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
        place_svg(self.source(), x, y, size, color, secondary)
    }

    /// The folder mark (the Regular outline, or the duotone's primary layer
    /// for a symbol without one), placed like [`Symbol::place`].
    fn place_mark(self, x: f32, y: f32, size: f32, color: &str) -> String {
        match self.mark_source() {
            Some(source) => place_svg(source, x, y, size, color, false),
            None => self.place(x, y, size, color, false),
        }
    }
}

/// An SVG document's contents, recoloured to `color` and placed at (x, y)
/// on the 32-unit grid, `size` units wide. `secondary: false` drops a
/// `#secondary` layer if there is one.
fn place_svg(source: &str, x: f32, y: f32, size: f32, color: &str, secondary: bool) -> String {
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
        s = size / SYMBOL_GRID
    )
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

/// The page: a rounded 20 × 27.5 sheet with no folded corner.
const PAGE: &str = "M10.5 2.5h11A4.5 4.5 0 0 1 26 7v18.5a4.5 4.5 0 0 1-4.5 4.5h-11A4.5 4.5 0 0 1 6 25.5V7a4.5 4.5 0 0 1 4.5-4.5Z";
/// The family-coloured header band across the page's top.
const BAND: &str = "M6 7a4.5 4.5 0 0 1 4.5-4.5h11A4.5 4.5 0 0 1 26 7v.5H6Z";
/// Where a card's extension sits (x, y, width, height), in units.
/// `ui::icon` sets the extension as type in the family's `label` colour,
/// straight on the page, centred in this box.
pub const LABEL_BOX: (f32, f32, f32, f32) = (6., 22.6, 20., 5.4);

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

/// A file icon: the page in the family's tints with a coloured header band
/// and the family symbol. A card's extension is laid over by the caller,
/// so one raster serves every extension in a family.
pub fn file_svg(family: FileFamily, ramp: &FileRamp, class: SizeClass, dark: bool) -> String {
    let ink = hex(ramp.ink);
    let symbol = Symbol::of_family(family);
    if class == SizeClass::Micro {
        let mark = match symbol {
            Some(s) => s.place(8., 10.5, 16., &ink, false),
            None => lines(&ink, &[(15., 12.), (20., 8.)], 2.2),
        };
        return document(&format!(
            r#"<path d="{PAGE}" fill="{fold}"/>{mark}"#,
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
        r#"<path d="{PAGE}" fill="url(#paper)"/><path d="{BAND}" fill="{fold}"/><path d="{PAGE}" fill="none" stroke="{edge}" stroke-opacity="{edge_alpha}" stroke-width=".75"/>"#,
        fold = hex(ramp.fold)
    );
    body += &match (symbol, card) {
        (Some(s), false) => s.place(8.5, 10.5, 15., &ink, true),
        (Some(s), true) => s.place(9.5, 9.6, 13., &ink, true),
        (None, false) => lines(&ink, &[(14., 12.), (18., 12.), (22., 7.)], 1.6),
        (None, true) => lines(&ink, &[(12., 12.), (15.4, 9.)], 1.3),
    };
    document(&body)
}

/// The folder's back (with its tab) and front panel, 26 × 22.5 units.
const FOLDER_BACK: &str = "M3 8.2A2.7 2.7 0 0 1 5.7 5.5h5.6c1 0 1.7.4 2.3 1.1l.9 1c.5.6 1.1.9 1.9.9h9.9A2.7 2.7 0 0 1 29 11.2v14.1a2.7 2.7 0 0 1-2.7 2.7H5.7A2.7 2.7 0 0 1 3 25.3Z";
const FOLDER_FRONT: &str = "M3 14.8a2.8 2.8 0 0 1 2.8-2.8h20.4a2.8 2.8 0 0 1 2.8 2.8v10.4a2.8 2.8 0 0 1-2.8 2.8H5.8A2.8 2.8 0 0 1 3 25.2Z";

/// A folder in the accent's ramp: one smooth front panel over a darker
/// back, no sheets, gloss or highlight line. The front's top edge meets
/// the back through a very soft shadow, and a special folder's mark is
/// debossed in the panel's own hue.
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
    body += &format!(r#"<path d="{FOLDER_BACK}" fill="url(#back)"/>"#);
    if !micro {
        body += &format!(
            r##"<path d="{FOLDER_FRONT}" transform="translate(0 -.5)" fill="#000000" fill-opacity="{}" filter="url(#soft)"/>"##,
            if dark { 0.28 } else { 0.1 }
        );
    }
    body += &format!(r#"<path d="{FOLDER_FRONT}" fill="url(#front)"/>"#);
    if let Some(symbol) = symbol.filter(|_| !micro) {
        let size = if card { 11.5 } else { 13. };
        let (x, y) = (16. - size / 2., 20. - size / 2.);
        body += &format!(
            r#"<g opacity=".6">{}</g>"#,
            symbol.place_mark(x, y, size, &hex(ramp.mark))
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

    /// Every symbol a special folder carries has a Regular mark, and the
    /// marks are single-layer outlines.
    #[test]
    fn every_folder_symbol_has_a_regular_mark() {
        let folders = [
            FolderIcon::Pictures,
            FolderIcon::Music,
            FolderIcon::Videos,
            FolderIcon::Documents,
            FolderIcon::Downloads,
            FolderIcon::Code,
            FolderIcon::Archives,
            FolderIcon::Desktop,
        ];
        for icon in folders {
            let symbol = Symbol::of_folder(icon).expect("special folders carry a symbol");
            let mark = symbol
                .mark_source()
                .unwrap_or_else(|| panic!("{symbol:?} has no mark"));
            assert!(!mark.contains("secondary"), "{symbol:?}");
            parses(mark);
        }
    }

    /// Folders press in the outline mark, not the duotone: no 20% interior
    /// layer reaches the folder artwork.
    #[test]
    fn folders_use_the_outline_mark() {
        let ramp = folder_ramp(gpui::rgb(0x087eaf), false);
        let svg = folder_svg(&ramp, Some(Symbol::Image), SizeClass::Card, false);
        assert!(!svg.contains("secondary"));
        let mark = Symbol::Image.mark_source().unwrap_or_default();
        let path = mark.split("d=\"").nth(1).and_then(|d| d.split('"').next());
        assert!(
            path.is_some_and(|d| svg.contains(d)),
            "the Regular image mark"
        );
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
                    parses(&file_svg(family, &ramp, class, dark));
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

    /// Micro icons are flat: no gradient, band or shadow at 16px.
    #[test]
    fn micro_icons_drop_fine_detail() {
        let ramp = file_ramp(FileFamily::Pdf, false);
        let micro = file_svg(FileFamily::Pdf, &ramp, SizeClass::Micro, false);
        assert!(!micro.contains("linearGradient") && !micro.contains(BAND));
        assert!(!micro.contains("url(#soft)"));
        let card = file_svg(FileFamily::Pdf, &ramp, SizeClass::Card, false);
        assert!(card.contains(BAND) && card.contains("url(#soft)"));
    }

    /// Pages are clean sheets: the header band carries the family colour and
    /// there's no folded corner or extension pill in the artwork (the
    /// extension is type laid over by `ui::icon`).
    #[test]
    fn pages_have_a_band_and_no_fold_or_pill() {
        let ramp = file_ramp(FileFamily::Spreadsheet, false);
        let card = file_svg(FileFamily::Spreadsheet, &ramp, SizeClass::Card, false);
        assert!(card.contains(&format!(r#"<path d="{BAND}" fill="{}"/>"#, hex(ramp.fold))));
        assert!(!card.contains("<rect x=\"8\""), "no extension pill");
        assert!(!card.contains("L27 9.5"), "no folded corner");
    }

    /// Folders are one soft panel over the back: no white gloss, paper
    /// sheets or highlight line, and the mark is debossed (partly
    /// transparent) rather than embossed with a white copy.
    #[test]
    fn folders_are_soft_panels_without_gloss() {
        for dark in [false, true] {
            let ramp = folder_ramp(gpui::rgb(0x087eaf), dark);
            let svg = folder_svg(&ramp, Some(Symbol::Video), SizeClass::Card, dark);
            assert!(
                !svg.contains("#ffffff"),
                "dark={dark}: no white highlight or sheet"
            );
            assert!(
                svg.contains(r#"<g opacity=".6">"#),
                "dark={dark}: debossed mark"
            );
            assert_eq!(svg.matches(FOLDER_FRONT).count(), 2, "shadow and panel");
        }
    }
}
