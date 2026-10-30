//! Icon colours (docs/design-icons.md): every file family and the folder
//! take their colours from one hue angle placed on fixed OKLCH lightness /
//! chroma steps, so no family is louder than another and every accent gives
//! an equally calm folder. Colours are computed, not spelled, which keeps
//! `theme.rs` the only UI file with hex literals; the tests below hold the
//! contrast targets so a later tweak can't make a family unreadable.

use gpui::Rgba;

use filex::file_types::FileFamily;

/// An OKLCH colour as (lightness 0..1, chroma, hue degrees).
type Lch = (f32, f32, f32);

fn oklch_to_linear((l, c, h): Lch) -> [f32; 3] {
    let (a, b) = (c * h.to_radians().cos(), c * h.to_radians().sin());
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
    [
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
    ]
}

fn encode(v: f32) -> f32 {
    let v = v.clamp(0., 1.);
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1. / 2.4) - 0.055
    }
}

fn decode(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// An sRGB colour for `lch`, lowering chroma until it fits the gamut (hue
/// and lightness are what the design fixes; chroma gives way first).
pub fn oklch((l, c, h): Lch) -> Rgba {
    let mut chroma = c;
    let mut rgb = oklch_to_linear((l, chroma, h));
    while chroma > 0. && rgb.iter().any(|v| !(-0.0005..=1.0005).contains(v)) {
        chroma -= 0.004;
        rgb = oklch_to_linear((l, chroma.max(0.), h));
    }
    Rgba::new(encode(rgb[0]), encode(rgb[1]), encode(rgb[2]), 1.)
}

/// The OKLCH hue (degrees) of an sRGB colour.
pub fn hue_of(color: Rgba) -> f32 {
    let [r, g, b] = [color.red, color.green, color.blue].map(decode);
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let b = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
    b.atan2(a).to_degrees().rem_euclid(360.)
}

/// WCAG relative luminance.
#[cfg(test)]
fn luminance(c: Rgba) -> f32 {
    let [r, g, b] = [c.red, c.green, c.blue].map(decode);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// WCAG contrast ratio between two opaque colours.
#[cfg(test)]
fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `#rrggbb` for embedding a colour in generated SVG.
pub fn hex(c: Rgba) -> String {
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(c.red),
        byte(c.green),
        byte(c.blue)
    )
}

/// A family's hue and how much of the full chroma it takes (neutral
/// families keep only a hint of colour).
fn family_hue(family: FileFamily) -> (f32, f32) {
    match family {
        FileFamily::Text => (255., 1.),
        FileFamily::Pdf => (27., 1.),
        FileFamily::Spreadsheet => (150., 1.),
        FileFamily::Presentation => (55., 1.),
        FileFamily::Image => (195., 1.),
        FileFamily::Video => (305., 1.),
        FileFamily::Audio => (355., 1.),
        FileFamily::Archive => (85., 1.),
        FileFamily::Code => (282., 1.),
        FileFamily::Font => (228., 0.6),
        FileFamily::App => (260., 0.17),
        FileFamily::Other => (260., 0.08),
    }
}

/// The colours one family's paper icon is drawn with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileRamp {
    pub paper_top: Rgba,
    pub paper_bottom: Rgba,
    /// The folded corner: the only coloured part of the paper.
    pub fold: Rgba,
    /// Hairline around the paper.
    pub edge: Rgba,
    /// The symbol.
    pub ink: Rgba,
    /// The extension label's fill (white text sits on it).
    pub label: Rgba,
}

/// (lightness, chroma) per role. Dark pages are dimmed a step so a folder
/// of documents doesn't glare on a near-black canvas.
const LIGHT_ROLES: [(f32, f32); 6] = [
    (0.988, 0.010),
    (0.958, 0.026),
    (0.885, 0.060),
    (0.620, 0.080),
    (0.575, 0.150),
    (0.530, 0.150),
];
const DARK_ROLES: [(f32, f32); 6] = [
    (0.940, 0.012),
    (0.900, 0.028),
    (0.800, 0.065),
    (0.300, 0.030),
    (0.545, 0.150),
    (0.515, 0.150),
];

pub fn file_ramp(family: FileFamily, dark: bool) -> FileRamp {
    let (hue, share) = family_hue(family);
    let roles = if dark { DARK_ROLES } else { LIGHT_ROLES };
    let at = |(l, c): (f32, f32)| oklch((l, c * share, hue));
    FileRamp {
        paper_top: at(roles[0]),
        paper_bottom: at(roles[1]),
        fold: at(roles[2]),
        edge: at(roles[3]),
        ink: at(roles[4]),
        label: at(roles[5]),
    }
}

/// The colours a folder is drawn with, all on the accent's hue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FolderRamp {
    pub back_top: Rgba,
    pub back_bottom: Rgba,
    pub front_top: Rgba,
    pub front_bottom: Rgba,
    /// The light catching the front panel's top edge.
    pub lip: Rgba,
    /// A pressed-in special-folder symbol.
    pub mark: Rgba,
    /// The rear sheet of paper showing in the opening.
    pub sheet: Rgba,
    pub edge: Rgba,
}

pub fn folder_ramp(accent: Rgba, dark: bool) -> FolderRamp {
    let hue = hue_of(accent);
    let at = |l: f32, c: f32| oklch((l, c, hue));
    if dark {
        FolderRamp {
            back_top: at(0.630, 0.130),
            back_bottom: at(0.540, 0.140),
            front_top: at(0.790, 0.125),
            front_bottom: at(0.680, 0.150),
            lip: at(0.900, 0.060),
            mark: at(0.430, 0.120),
            sheet: at(0.900, 0.020),
            edge: at(0.200, 0.030),
        }
    } else {
        FolderRamp {
            back_top: at(0.690, 0.135),
            back_bottom: at(0.600, 0.145),
            front_top: at(0.840, 0.120),
            front_bottom: at(0.730, 0.150),
            lip: at(0.940, 0.050),
            mark: at(0.470, 0.130),
            sheet: at(0.955, 0.020),
            edge: at(0.420, 0.080),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white() -> Rgba {
        Rgba::new(1., 1., 1., 1.)
    }

    /// The design's promise: every symbol reads on its paper (3:1, the
    /// WCAG graphics target) and every label's white text reads on its
    /// fill (4.5:1), in both themes.
    #[test]
    fn every_family_meets_its_contrast_targets() {
        for dark in [false, true] {
            for family in FileFamily::ALL {
                let ramp = file_ramp(family, dark);
                let symbol = contrast(ramp.ink, ramp.paper_bottom);
                let label = contrast(white(), ramp.label);
                assert!(symbol >= 3.0, "{family:?} dark={dark}: symbol {symbol:.2}");
                assert!(label >= 4.5, "{family:?} dark={dark}: label {label:.2}");
            }
        }
    }

    /// Hues are what tells families apart: no two coloured families may
    /// share one.
    #[test]
    fn coloured_families_have_distinct_hues() {
        let hues: Vec<f32> = FileFamily::ALL
            .iter()
            .map(|&f| family_hue(f))
            .filter(|&(_, share)| share >= 0.5)
            .map(|(hue, _)| hue)
            .collect();
        for (i, a) in hues.iter().enumerate() {
            for b in &hues[i + 1..] {
                let gap = (a - b).abs().min(360. - (a - b).abs());
                assert!(gap >= 20., "hues {a} and {b} are only {gap}° apart");
            }
        }
    }

    #[test]
    fn out_of_gamut_colours_clamp_into_srgb() {
        let c = oklch((0.6, 0.4, 150.));
        for v in [c.red, c.green, c.blue] {
            assert!((0. ..=1.).contains(&v));
        }
    }

    #[test]
    fn hue_round_trips() {
        for hue in [27., 150., 255., 305.] {
            let back = hue_of(oklch((0.6, 0.1, hue)));
            assert!((back - hue).abs() < 1.5, "{hue} came back as {back}");
        }
    }

    /// The folder's front sits a step in front of (lighter than) its back,
    /// and the mark is darker than the panel it's pressed into, for any
    /// accent.
    #[test]
    fn folders_keep_their_depth_for_every_accent() {
        for accent in [0x087eaf, 0x8b5cf6, 0x22c55e, 0xf97316, 0xef4444] {
            for dark in [false, true] {
                let ramp = folder_ramp(gpui::rgb(accent), dark);
                assert!(luminance(ramp.front_top) > luminance(ramp.back_top));
                assert!(
                    contrast(ramp.mark, ramp.front_top) >= 3.0,
                    "{accent:x} dark={dark}"
                );
            }
        }
    }

    #[test]
    fn hex_formats_six_digits() {
        assert_eq!(hex(gpui::rgb(0x0a8b3f)), "#0a8b3f");
    }
}
