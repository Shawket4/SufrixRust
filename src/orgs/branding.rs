//! Brand colours read out of an organisation's logo.
//!
//! A shop uploads one logo and gets a themed loyalty card — no colour pickers,
//! nothing else to configure, and no way to choose two colours nobody can read.
//! The palette is DERIVED, so it cannot be set wrong.
//!
//! The maths here is pure and unit-tested. Deriving a palette means decoding an
//! image, which has no business happening while a customer waits for their card,
//! so the result is cached on the organisation row against the logo it came
//! from — computed once at upload, and healed on first read for a logo that
//! predates the column ([`load`]).

use image::GenericImageView;

/// Madar's own, used when there is no logo or nothing usable in it.
pub const MADAR_TEAL: &str = "#0D6273";
pub const MADAR_TEAL_LIGHT: &str = "#2E94A6";
pub const MADAR_PAPER: &str = "#EFF3F4";
/// Ink for a light ground.
pub const MADAR_INK: &str = "#12222A";

/// The three colours a card is painted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// The card's ground — the logo's dominant colour.
    pub background: String,
    /// Text on that ground. Chosen for contrast, never sampled.
    pub foreground: String,
    /// Filled steps and accents.
    pub accent: String,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: MADAR_TEAL.into(),
            foreground: MADAR_PAPER.into(),
            accent: MADAR_TEAL_LIGHT.into(),
        }
    }
}

pub fn hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Relative luminance, per WCAG 2.1.
pub fn luminance(r: u8, g: u8, b: u8) -> f64 {
    let f = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// Contrast ratio between two luminances, per WCAG. 1.0 = identical, 21.0 = max.
fn contrast(a: f64, b: f64) -> f64 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// Text that is actually readable on `(r,g,b)`.
///
/// Picked by measurement, not by taste: whichever of ink or paper has the
/// better contrast wins. A sampled second colour from the logo would look
/// considered and be unreadable about half the time.
pub fn readable_on(r: u8, g: u8, b: u8) -> String {
    let bg = luminance(r, g, b);
    let ink = luminance(0x12, 0x22, 0x2A);
    let paper = luminance(0xEF, 0xF3, 0xF4);
    if contrast(bg, ink) >= contrast(bg, paper) {
        MADAR_INK.into()
    } else {
        MADAR_PAPER.into()
    }
}

/// The AA floor for body text. A brand colour is not worth a card nobody can
/// read off a phone in daylight.
const MIN_CONTRAST: f64 = 4.5;

/// Move a ground until its best text colour clears [`MIN_CONTRAST`], keeping the
/// hue.
///
/// Some perfectly ordinary brand colours — a vivid blue like `#0066FF` — clear
/// AA against NEITHER dark ink nor light paper: 4.33:1 either way. Picking the
/// better of the two is not enough, so the ground is darkened or lightened a
/// step at a time, in the direction that already had more headroom, until the
/// text clears. The shop still gets their colour; the customer still gets a
/// card they can read.
fn ensure_readable(r: u8, g: u8, b: u8) -> (u8, u8, u8, String) {
    let ink = luminance(0x12, 0x22, 0x2A);
    let paper = luminance(0xEF, 0xF3, 0xF4);
    let (mut r, mut g, mut b) = (r, g, b);

    // Whichever direction has more room to begin with is the one to commit to;
    // alternating would oscillate without converging.
    let go_darker = {
        let l = luminance(r, g, b);
        contrast(l, paper) >= contrast(l, ink)
    };

    // 24 steps is far more than enough to cross the range; the bound is here so
    // a pathological input cannot spin.
    for _ in 0..24 {
        let l = luminance(r, g, b);
        let best = contrast(l, ink).max(contrast(l, paper));
        if best >= MIN_CONTRAST {
            break;
        }
        let f = |c: u8| {
            if go_darker {
                (c as f64 * 0.90).round() as u8
            } else {
                (c as f64 + (255.0 - c as f64) * 0.10).round() as u8
            }
        };
        let (nr, ng, nb) = (f(r), f(g), f(b));
        if (nr, ng, nb) == (r, g, b) {
            break; // already at an extreme
        }
        (r, g, b) = (nr, ng, nb);
    }
    let fg = readable_on(r, g, b);
    (r, g, b, fg)
}

/// Nudge a colour toward the light or dark end, for the accent.
fn shift(r: u8, g: u8, b: u8, lighter: bool) -> (u8, u8, u8) {
    let f = |c: u8| {
        if lighter {
            (c as f64 + (255.0 - c as f64) * 0.42).round() as u8
        } else {
            (c as f64 * 0.62).round() as u8
        }
    };
    (f(r), f(g), f(b))
}

/// The dominant colour of an already-decoded image, as a palette.
///
/// Deliberately ignores three things, in this order:
///   * **transparent pixels** — most logos are a mark on nothing, and averaging
///     in the empty space returns grey every time;
///   * **near-white and near-black** — the paper a mark sits on and its outline
///     are not the brand;
///   * **near-grey** — a shadow or a border, which would beat a small vivid mark
///     on count alone.
///
/// Returns `None` when nothing survives, so the caller falls back rather than
/// painting a card in whatever grey was left.
pub fn palette_from_image(img: &image::DynamicImage) -> Option<Palette> {
    // Coarse buckets: exact-colour counting loses to anti-aliasing, where every
    // pixel of a flat logo is a slightly different value.
    const BUCKET: u32 = 24;
    /// A bucket's key: the quantised colour.
    type Bucket = (u32, u32, u32);
    /// What we accumulate per bucket: how many pixels, and their channel sums,
    /// so the winner can be the bucket's MEAN rather than its corner.
    type Tally = (u64, u64, u64, u64);
    let mut counts: std::collections::HashMap<Bucket, Tally> = std::collections::HashMap::new();

    // Downscaled with NEAREST, not a smoothing filter: interpolation invents
    // colours that are in no part of the logo, and a dominant-colour search
    // should only ever see pixels the designer actually put there. Also bounds
    // the work regardless of what was uploaded.
    let (w, h) = img.dimensions();
    let small = if w > 96 || h > 96 {
        img.resize_exact(
            w.clamp(1, 96),
            h.clamp(1, 96),
            image::imageops::FilterType::Nearest,
        )
    } else {
        img.clone()
    };
    for (_, _, px) in small.pixels() {
        let [r, g, b, a] = px.0;
        if a < 128 {
            continue;
        }
        let (rf, gf, bf) = (r as f64, g as f64, b as f64);
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        // Near-white / near-black.
        if max > 240.0 && min > 240.0 {
            continue;
        }
        if max < 26.0 {
            continue;
        }
        // Near-grey: little separation between channels means no hue to take.
        if max - min < 18.0 {
            continue;
        }
        let key = (r as u32 / BUCKET, g as u32 / BUCKET, b as u32 / BUCKET);
        let e = counts.entry(key).or_insert((0, 0, 0, 0));
        e.0 += r as u64;
        e.1 += g as u64;
        e.2 += b as u64;
        e.3 += 1;
    }

    let (_, (sr, sg, sb, n)) = counts.into_iter().max_by_key(|(_, v)| v.3)?;
    if n == 0 {
        return None;
    }
    // The bucket's mean, not its midpoint — truer to the actual mark. Rounded,
    // not truncated: truncation biases every channel down, which is how a
    // #0D6273 logo produced a #0D6272 card.
    let mean = |sum: u64| (sum as f64 / n as f64).round() as u8;
    let (r, g, b) = (mean(sr), mean(sg), mean(sb));

    // The logo's colour, moved only as far as legibility requires.
    let (r, g, b, foreground) = ensure_readable(r, g, b);
    let dark_ground = luminance(r, g, b) < 0.5;
    let (ar, ag, ab) = shift(r, g, b, dark_ground);
    Some(Palette {
        background: hex(r, g, b),
        foreground,
        accent: hex(ar, ag, ab),
    })
}

/// Is this logo a MARK — a shape on transparency — or an opaque tile?
///
/// It decides how the logo may be drawn, and the two answers are opposites.
///
/// A mark can be recoloured: paint every opaque pixel in the card's foreground
/// and it becomes a silhouette that is legible on the card BY CONSTRUCTION,
/// because the foreground is the one colour already guaranteed to clear AA on
/// that ground. This is what a card wants, since the ground is derived from the
/// logo's own dominant colour — so a logo drawn in its own colours is, almost
/// by definition, the colour it is sitting on. Blue on blue.
///
/// A logo with its background baked in cannot be recoloured: every pixel is
/// opaque, so the silhouette is a solid rectangle. That one gets a plate to sit
/// on instead.
///
/// The test is transparency, measured over the whole image rather than guessed
/// at from the corners: a mark leaves a lot of the frame empty, and a photo or
/// a baked tile leaves almost none.
pub fn is_mark(img: &image::DynamicImage) -> bool {
    const CLEAR: u8 = 16;
    /// A mark's frame is mostly empty. A tile's is not remotely.
    const ENOUGH: f64 = 0.10;

    let rgba = img.to_rgba8();
    let total = (rgba.width() as u64) * (rgba.height() as u64);
    if total == 0 {
        return false;
    }
    let clear = rgba.pixels().filter(|p| p.0[3] < CLEAR).count() as f64;
    clear / total as f64 > ENOUGH
}

/// Repaint a mark in one colour, keeping its alpha.
///
/// Alpha is what carries the shape, so anti-aliased edges survive and the
/// result reads as the same logo rather than a traced one. Only ever called on
/// something [`is_mark`] agreed to.
pub fn tint_mark(img: &image::DynamicImage, hex: &str) -> image::DynamicImage {
    let (r, g, b) = parse_hex(hex).unwrap_or((255, 255, 255));
    let mut rgba = img.to_rgba8();
    for p in rgba.pixels_mut() {
        p.0 = [r, g, b, p.0[3]];
    }
    image::DynamicImage::ImageRgba8(rgba)
}

/// `#RRGGBB` to its channels.
pub fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    if hex.len() != 7 || !hex.starts_with('#') {
        return None;
    }
    let c = |a: usize, b: usize| u8::from_str_radix(&hex[a..b], 16).ok();
    Some((c(1, 3)?, c(3, 5)?, c(5, 7)?))
}

/// Who a shop is, everywhere a customer sees them.
///
/// One row, one loader. The web card, the Apple pass and the Google pass all
/// paint the same organisation, and each having its own copy of this query is
/// how they came to disagree — the pass kept reading colours out of
/// `loyalty_settings` long after the UI for them was removed, so every pass came
/// back Apple's default grey while the web card was correctly themed.
#[derive(Debug, Clone, Default)]
pub struct OrgBrand {
    /// The organisation's name, as it appears in a customer's wallet list.
    /// Empty when the org has somehow gone missing, which callers fall back on.
    pub name: String,
    /// Absolute URL of the logo, for the surfaces that FETCH an image (the web
    /// card, Google Wallet). Apple embeds bytes instead — see
    /// `loyalty::wallet::apple::pass_brand`.
    pub logo_url: Option<String>,
    /// Derived from the logo when it was uploaded; Madar's own until then.
    pub palette: Palette,
    /// True when [`is_mark`] says the logo can be recoloured for contrast.
    /// False for an opaque tile, and for no logo at all.
    pub logo_is_mark: bool,
}

/// Read an organisation's brand.
///
/// A missing organisation, or a row whose colours predate the palette work,
/// yields Madar's own rather than nothing: an unbranded card is a fallback, a
/// blank one is a bug.
pub async fn load(pool: &sqlx::PgPool, org_id: uuid::Uuid) -> Result<OrgBrand, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        name: String,
        logo_url: Option<String>,
        brand_background: Option<String>,
        brand_foreground: Option<String>,
        brand_accent: Option<String>,
        brand_logo_is_mark: Option<bool>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT name, logo_url, brand_background, brand_foreground, brand_accent, \
                brand_logo_is_mark \
           FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(OrgBrand::default());
    };

    // Healed on first read, not backfilled by an operator. The column arrived
    // after these logos did, and a shop should not have to re-upload its mark
    // to get a card that reads. One decode per organisation, ever.
    let logo_is_mark = match (row.brand_logo_is_mark, row.logo_url.as_deref()) {
        (Some(known), _) => known,
        (None, Some(url)) => {
            let mark = read_logo(url).map(|img| is_mark(&img)).unwrap_or(false);
            let _ = sqlx::query("UPDATE organizations SET brand_logo_is_mark = $2 WHERE id = $1")
                .bind(org_id)
                .bind(mark)
                .execute(pool)
                .await;
            mark
        }
        (None, None) => false,
    };

    let d = Palette::default();
    Ok(OrgBrand {
        name: row.name,
        logo_url: row.logo_url,
        palette: Palette {
            background: row.brand_background.unwrap_or(d.background),
            foreground: row.brand_foreground.unwrap_or(d.foreground),
            accent: row.brand_accent.unwrap_or(d.accent),
        },
        logo_is_mark,
    })
}

/// Decode an organisation's logo from the uploads directory.
///
/// From DISK, never fetched: the file is already local, so there is no network
/// call while a customer waits and no server-side request to an address someone
/// else supplied. Returns `None` for anything unreadable, which every caller
/// treats as "no logo" rather than as an error.
pub fn read_logo(logo_url: &str) -> Option<image::DynamicImage> {
    let file = logo_url.rsplit_once("/logos/")?.1;
    if file.is_empty() || file.contains('/') || file.contains("..") {
        return None;
    }
    let dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".into());
    let bytes = std::fs::read(format!("{dir}/logos/{file}")).ok()?;
    image::load_from_memory(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgba, RgbaImage};

    fn solid(w: u32, h: u32, px: [u8; 4]) -> DynamicImage {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgba(px);
        }
        DynamicImage::ImageRgba8(img)
    }

    #[test]
    fn a_solid_mark_gives_its_own_colour() {
        let p = palette_from_image(&solid(64, 64, [13, 98, 115, 255])).unwrap();
        assert_eq!(p.background, "#0D6273");
        // Dark ground → light text.
        assert_eq!(p.foreground, MADAR_PAPER);
    }

    #[test]
    fn text_is_chosen_for_contrast_not_taste() {
        // A pale yellow logo must NOT get white text.
        let p = palette_from_image(&solid(32, 32, [250, 224, 96, 255])).unwrap();
        assert_eq!(p.foreground, MADAR_INK, "light ground needs dark ink");
        let p = palette_from_image(&solid(32, 32, [20, 30, 90, 255])).unwrap();
        assert_eq!(p.foreground, MADAR_PAPER, "dark ground needs light text");
    }

    #[test]
    fn transparency_is_ignored_so_a_mark_on_nothing_still_reads() {
        // A small red mark on a fully transparent field: the mark is the brand,
        // and averaging in the empty space would return grey.
        let mut img = RgbaImage::new(64, 64);
        for p in img.pixels_mut() {
            *p = Rgba([0, 0, 0, 0]);
        }
        for y in 20..44 {
            for x in 20..44 {
                img.put_pixel(x, y, Rgba([200, 32, 40, 255]));
            }
        }
        let p = palette_from_image(&DynamicImage::ImageRgba8(img)).unwrap();
        let r = u8::from_str_radix(&p.background[1..3], 16).unwrap();
        assert!(r > 150, "the mark's red should win, got {}", p.background);
    }

    #[test]
    fn a_logo_with_no_colour_falls_back_rather_than_painting_it_grey() {
        // Pure black-and-white marks are common, and a grey card is worse than
        // the Madar one.
        assert!(palette_from_image(&solid(32, 32, [255, 255, 255, 255])).is_none());
        assert!(palette_from_image(&solid(32, 32, [10, 10, 10, 255])).is_none());
        assert!(palette_from_image(&solid(32, 32, [128, 128, 128, 255])).is_none());
        // And the default is Madar's, never something derived from nothing.
        assert_eq!(Palette::default().background, MADAR_TEAL);
    }

    #[test]
    fn an_empty_image_does_not_panic() {
        assert!(palette_from_image(&solid(1, 1, [0, 0, 0, 0])).is_none());
    }

    #[test]
    fn every_derived_palette_is_actually_readable() {
        // The point of deriving rather than letting someone pick: the text must
        // clear the AA floor on whatever ground the logo produced. Swept across
        // the colour cube so this holds for logos nobody has uploaded yet.
        for r in (0u16..=255).step_by(51) {
            for g in (0u16..=255).step_by(51) {
                for b in (0u16..=255).step_by(51) {
                    let (r, g, b) = (r as u8, g as u8, b as u8);
                    let Some(p) = palette_from_image(&solid(8, 8, [r, g, b, 255])) else {
                        continue; // greys and extremes fall back by design
                    };
                    // Measured against the palette's OWN background, which is
                    // the contract: `palette_from_image` may move the ground to
                    // reach legibility, and what ships is what must be read.
                    let parse = |h: &str, i: usize| {
                        u8::from_str_radix(&h[1 + i * 2..3 + i * 2], 16).unwrap()
                    };
                    let (br, bg_, bb) = (
                        parse(&p.background, 0),
                        parse(&p.background, 1),
                        parse(&p.background, 2),
                    );
                    let fg = if p.foreground == MADAR_INK {
                        (0x12u8, 0x22u8, 0x2Au8)
                    } else {
                        (0xEFu8, 0xF3u8, 0xF4u8)
                    };
                    let ratio = {
                        let a = luminance(br, bg_, bb);
                        let c = luminance(fg.0, fg.1, fg.2);
                        let (hi, lo) = if a > c { (a, c) } else { (c, a) };
                        (hi + 0.05) / (lo + 0.05)
                    };
                    assert!(
                        ratio >= 4.5,
                        "logo #{r:02X}{g:02X}{b:02X} -> card {} on {} is only {ratio:.2}:1",
                        p.foreground,
                        p.background
                    );
                }
            }
        }
    }
}
