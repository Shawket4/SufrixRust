//! Apple Wallet store cards (`.pkpass`).
//!
//! A `.pkpass` is a zip of `pass.json`, a `manifest.json` of SHA-1 digests, and
//! a PKCS#7 detached signature over that manifest, made with the Pass Type ID
//! certificate. Everything here is built and testable today; the signature is
//! the one step that cannot exist until Madar holds an Apple Developer account,
//! and it is isolated in [`sign_manifest`] so that day is a small change.
//!
//! Configured by `LOYALTY_APPLE_PASS_TYPE_ID`, `LOYALTY_APPLE_TEAM_ID`,
//! `LOYALTY_APPLE_CERT_PEM`, `LOYALTY_APPLE_KEY_PEM` and `LOYALTY_APPLE_WWDR_PEM`.
//! With any of them unset there is no Apple button at all — signup still works
//! and the member still has a token and a QR.

use serde_json::json;
use sqlx::PgPool;

use crate::errors::AppError;
use crate::loyalty::model::MemberRow;
use crate::loyalty::settings::LoyaltySettings;

pub use super::{PassLocation, locations_for_org};

use super::google::progress_line;

/// The images every pass carries, compiled into the binary.
///
/// **`icon.png` is not optional.** A pass without one is rejected by iOS
/// outright, and the only thing the customer sees is Safari saying it "cannot
/// download this file" — no mention of an icon, nothing in any log. The rest
/// are what make the pass look like Madar rather than a grey rectangle.
///
/// Embedded rather than read from disk so a pass can never be half-built by a
/// missing file on one deploy, and so the manifest always covers exactly what
/// ships. Every entry here MUST be hashed into `manifest.json` — an
/// unhashed file in the archive invalidates the signature.
const PASS_IMAGES: &[(&str, &[u8])] = &[
    (
        "icon.png",
        include_bytes!("../../../static/wallet/icon.png"),
    ),
    (
        "icon@2x.png",
        include_bytes!("../../../static/wallet/icon@2x.png"),
    ),
    (
        "icon@3x.png",
        include_bytes!("../../../static/wallet/icon@3x.png"),
    ),
    (
        "logo.png",
        include_bytes!("../../../static/wallet/logo.png"),
    ),
    (
        "logo@2x.png",
        include_bytes!("../../../static/wallet/logo@2x.png"),
    ),
    (
        "logo@3x.png",
        include_bytes!("../../../static/wallet/logo@3x.png"),
    ),
];

/// The organisation's identity, as the PASS must carry it.
///
/// A pass is a file: it cannot reference a logo by URL the way the web card
/// does, so the image has to be resized and packed into the archive. And its
/// colours cannot come from `loyalty_settings` — that is where they used to
/// live, before branding moved to the organisation, which is exactly why a
/// freshly downloaded pass kept coming back in Apple's default grey.
pub struct PassBrand {
    pub org_name: String,
    pub background: String,
    pub foreground: String,
    pub label: String,
    /// The org's logo, already sized for the pass. Madar's is used when the
    /// shop has none.
    pub images: Vec<(String, Vec<u8>)>,
}

impl Default for PassBrand {
    fn default() -> Self {
        let p = crate::orgs::branding::Palette::default();
        Self {
            org_name: String::new(),
            background: p.background,
            foreground: p.foreground,
            label: p.accent,
            images: PASS_IMAGES
                .iter()
                .map(|(n, b)| ((*n).to_string(), b.to_vec()))
                .collect(),
        }
    }
}

/// Resize the org's logo into the image set a pass needs.
///
/// Apple's sizes, and both are required: `icon` (29pt, shown in notifications
/// and on the lock screen — a pass without it is refused outright) and `logo`
/// (up to 160×50pt in the header). Aspect ratio is preserved for the logo and
/// the icon is squared, because a stretched mark is worse than Madar's.
///
/// `tint` repaints a MARK so it reads on the pass. The pass's ground is derived
/// from the logo's own dominant colour, so a logo left in its own colours is
/// very nearly the colour it is sitting on; the foreground is the one colour
/// already guaranteed to clear AA on that ground. Passed `None` for a logo with
/// its background baked in, where repainting would give a solid rectangle.
fn images_from_logo(
    img: &image::DynamicImage,
    tint: Option<&str>,
) -> Option<Vec<(String, Vec<u8>)>> {
    let owned;
    let img = match tint {
        Some(hex) => {
            owned = crate::orgs::branding::tint_mark(img, hex);
            &owned
        }
        None => img,
    };
    let mut out = Vec::with_capacity(6);
    let encode = |im: image::DynamicImage| -> Option<Vec<u8>> {
        let mut buf = std::io::Cursor::new(Vec::new());
        im.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some(buf.into_inner())
    };
    for (name, px) in [
        ("icon.png", 29u32),
        ("icon@2x.png", 58),
        ("icon@3x.png", 87),
    ] {
        out.push((
            name.to_string(),
            encode(img.resize_to_fill(px, px, image::imageops::FilterType::Lanczos3))?,
        ));
    }
    for (name, w, h) in [
        ("logo.png", 160u32, 50u32),
        ("logo@2x.png", 320, 100),
        ("logo@3x.png", 480, 150),
    ] {
        out.push((
            name.to_string(),
            encode(img.resize(w, h, image::imageops::FilterType::Lanczos3))?,
        ));
    }
    Some(out)
}

/// Dress a pass in an organisation's brand.
///
/// The logo is read from DISK, not fetched: uploads are written locally, so the
/// file is already there — no network call while a customer waits, and no
/// server-side request to an address someone else supplied.
pub fn pass_brand(brand: &crate::orgs::branding::OrgBrand) -> PassBrand {
    let d = PassBrand::default();
    let images = brand
        .logo_url
        .as_deref()
        .and_then(crate::orgs::branding::read_logo)
        .and_then(|img| {
            // A mark is repainted in the pass's own foreground; a baked tile is
            // left alone, because a silhouette of it is just a rectangle.
            let tint = brand
                .logo_is_mark
                .then_some(brand.palette.foreground.as_str());
            images_from_logo(&img, tint)
        })
        .unwrap_or(d.images);

    PassBrand {
        org_name: brand.name.clone(),
        background: brand.palette.background.clone(),
        foreground: brand.palette.foreground.clone(),
        label: brand.palette.accent.clone(),
        images,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Shared with Google — see [`super::key_material`].
use super::key_material as pem_material;

pub fn pass_type_id() -> Option<String> {
    env_nonempty("LOYALTY_APPLE_PASS_TYPE_ID")
}

pub fn team_id() -> Option<String> {
    env_nonempty("LOYALTY_APPLE_TEAM_ID")
}

pub fn is_configured() -> bool {
    pass_type_id().is_some()
        && team_id().is_some()
        && pem_material("LOYALTY_APPLE_CERT_PEM").is_some()
        && pem_material("LOYALTY_APPLE_KEY_PEM").is_some()
        && pem_material("LOYALTY_APPLE_WWDR_PEM").is_some()
}

/// The pass as Apple models it.
///
/// Field layout, as decided: the **balance** is the primary field (it is what
/// staff and customer reconcile against), the **progress** line is secondary,
/// the member's **name** is auxiliary, and the how-it-works, reward list and
/// terms go on the back. The barcode is the member token, with the member's name
/// as `altText` so a teller can eyeball that they scanned the right card.
pub fn pass_json(
    member: &MemberRow,
    settings: &LoyaltySettings,
    locations: &[PassLocation],
    rewards: &[String],
    brand: &PassBrand,
) -> Result<serde_json::Value, AppError> {
    let (Some(pass_type), Some(team)) = (pass_type_id(), team_id()) else {
        return Err(AppError::ServiceUnavailable(
            "Apple Wallet is not configured".into(),
        ));
    };
    let program = &settings.program_name;
    let mode = settings.mode();
    let threshold = settings.default_reward_cost;
    let balance = member.balance_in(mode);

    let mut back = vec![
        json!({
            "key": "howitworks",
            "label": "How it works",
            // The two programs are explained in their own terms — a stamp card
            // that talked about EGP per point would be a card nobody could
            // follow at the counter.
            "value": match mode {
                crate::loyalty::earn::Mode::Points => format!(
                    "Show this card when you pay. You earn a point for every {} EGP you spend, \
                     and a reward costs {threshold} points.",
                    settings.earn_piastres_per_point / 100,
                ),
                crate::loyalty::earn::Mode::Visits => format!(
                    "Show this card when you pay. Every order earns a stamp, \
                     and a reward costs {threshold} of them."
                ),
            }
        }),
        json!({
            "key": "member",
            "label": "Member",
            "value": format!("{} · {}", member.name, member.phone)
        }),
    ];
    if !rewards.is_empty() {
        back.push(json!({
            "key": "rewards",
            "label": "Rewards you can claim",
            "value": rewards.join("\n")
        }));
    }
    if !locations.is_empty() {
        back.push(json!({
            "key": "branches",
            "label": "Where it works",
            "value": locations.iter().map(|l| l.name.clone()).collect::<Vec<_>>().join("\n")
        }));
    }
    if let Some(terms) = &settings.terms {
        back.push(json!({ "key": "terms", "label": "Terms", "value": terms }));
    }

    let mut pass = json!({
        "formatVersion": 1,
        "passTypeIdentifier": pass_type,
        "teamIdentifier": team,
        // Stable per member: re-issuing must update the card already in the
        // customer's wallet, never add a second one.
        "serialNumber": member.id.to_string(),
        // The SHOP's name, not the programme's. This is the line a customer
        // sees in their wallet list and on the lock screen; "Rewards" there
        // tells them nothing about whose card it is.
        "organizationName": if brand.org_name.trim().is_empty() { program.as_str() } else { brand.org_name.as_str() },
        "description": format!("{program} card"),
        // The web service that serves updates. Apple only calls it when the
        // pass carries an auth token, which is minted at signup.
        "authenticationToken": member.apple_auth_token,
        "storeCard": {
            // The strip a customer sees WITHOUT opening the pass. Wallet stacks
            // cards and shows only this row, so a pass with no header fields
            // answers "how many do I have?" with nothing until you tap it.
            "headerFields": [{
                "key": "header",
                "label": super::google::balance_label(mode),
                "value": balance
            }],
            "primaryFields": [{
                "key": "balance",
                "label": super::google::balance_label(mode),
                "value": balance
            }],
            "secondaryFields": [{
                "key": "progress",
                "label": program,
                "value": progress_line(balance, threshold)
            }],
            // What they are working towards. This row used to repeat the
            // member's name, which is already under the barcode and on the
            // back — three times on one card, and none of them the thing a
            // customer is actually counting for.
            "auxiliaryFields": [{
                "key": "reward",
                "label": "Next reward",
                "value": rewards.first().cloned().unwrap_or_else(|| format!(
                    "{threshold} {}",
                    super::google::balance_label(mode).to_lowercase()
                ))
            }],
            "backFields": back
        },
        "barcodes": [{
            "format": "PKBarcodeFormatQR",
            "message": member.member_token,
            "messageEncoding": "iso-8859-1",
            "altText": member.name
        }]
    });

    // Apple appends `/v1/...` itself, so this is the scope's parent. Without it
    // the device never registers and the pass can never update — which is
    // permanent for every pass already issued, so it is worth falling back to
    // the API's own origin rather than depending on one variable.
    if let Some(url) = super::web_service_url() {
        pass["webServiceURL"] = json!(url);
    }
    // From the ORGANISATION's derived palette. These used to read
    // `loyalty_settings`, which stopped being written when branding moved to
    // the org — so every pass came back in Apple's default grey however the
    // shop's card looked on the web.
    pass["backgroundColor"] = json!(hex_to_rgb_css(&brand.background));
    pass["foregroundColor"] = json!(hex_to_rgb_css(&brand.foreground));
    pass["labelColor"] = json!(hex_to_rgb_css(&brand.label));
    if !locations.is_empty() {
        pass["locations"] = json!(
            locations
                .iter()
                .map(|l| json!({
                    "latitude": l.latitude,
                    "longitude": l.longitude,
                    "relevantText": format!("{program} — you're near {}", l.name)
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(pass)
}

/// Apple wants `rgb(r, g, b)`, not `#rrggbb`. An unparseable colour is dropped
/// rather than shipped — a malformed value makes iOS reject the whole pass.
fn hex_to_rgb_css(hex: &str) -> String {
    let parse = |s: &str| u8::from_str_radix(s, 16).ok();
    let ok = hex.len() == 7
        && hex.starts_with('#')
        && parse(&hex[1..3]).is_some()
        && parse(&hex[3..5]).is_some()
        && parse(&hex[5..7]).is_some();
    if !ok {
        return "rgb(255, 255, 255)".to_string();
    }
    format!(
        "rgb({}, {}, {})",
        parse(&hex[1..3]).unwrap(),
        parse(&hex[3..5]).unwrap(),
        parse(&hex[5..7]).unwrap()
    )
}

/// PKCS#7 detached signature over `manifest.json`, made with the Pass Type ID
/// certificate and chained through Apple's WWDR intermediate.
///
/// Detached and binary, in DER: iOS verifies this blob against `manifest.json`
/// and refuses the pass — with no message the customer or the operator can see —
/// if anything about it is off. That silent failure is why this uses OpenSSL's
/// `PKCS7_sign` rather than a hand-rolled CMS structure.
///
/// The WWDR intermediate goes in as a chain certificate, not a signer: leaving it
/// out produces a signature that verifies on a Mac (which has WWDR installed) and
/// fails on a customer's phone, which is the worst way to find a bug.
fn sign_manifest(manifest: &[u8]) -> Result<Vec<u8>, AppError> {
    use openssl::pkcs7::{Pkcs7, Pkcs7Flags};
    use openssl::pkey::PKey;
    use openssl::stack::Stack;
    use openssl::x509::X509;

    let missing = |what: &str| {
        AppError::ServiceUnavailable(format!("Apple Wallet is not configured: {what} is missing"))
    };
    let cert_pem =
        pem_material("LOYALTY_APPLE_CERT_PEM").ok_or_else(|| missing("the certificate"))?;
    let key_pem =
        pem_material("LOYALTY_APPLE_KEY_PEM").ok_or_else(|| missing("the private key"))?;
    let wwdr_pem = pem_material("LOYALTY_APPLE_WWDR_PEM")
        .ok_or_else(|| missing("the Apple WWDR intermediate"))?;

    let bad = |what: &str, e: openssl::error::ErrorStack| {
        tracing::error!(error = %e, "Apple Wallet: {what} is not usable");
        AppError::ServiceUnavailable(format!("Apple Wallet {what} is not usable"))
    };
    let cert = X509::from_pem(&cert_pem).map_err(|e| bad("certificate", e))?;
    let key = PKey::private_key_from_pem(&key_pem).map_err(|e| bad("private key", e))?;
    let wwdr = X509::from_pem(&wwdr_pem).map_err(|e| bad("WWDR certificate", e))?;

    let mut chain = Stack::new().map_err(|e| bad("certificate chain", e))?;
    chain.push(wwdr).map_err(|e| bad("certificate chain", e))?;

    // DETACHED: the signature covers the manifest without embedding it.
    // BINARY: no MIME canonicalisation, so the bytes signed are the bytes
    // shipped — a CRLF rewrite here would break verification on the device.
    let signed = Pkcs7::sign(
        &cert,
        &key,
        &chain,
        manifest,
        Pkcs7Flags::DETACHED | Pkcs7Flags::BINARY,
    )
    .map_err(|e| bad("signature", e))?;
    signed.to_der().map_err(|e| bad("signature", e))
}

/// Assemble the `.pkpass` archive: `pass.json`, its manifest of SHA-1 digests,
/// and the detached signature over that manifest.
pub fn build_pkpass(
    pass: &serde_json::Value,
    images: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, AppError> {
    use sha1::{Digest, Sha1};
    use std::io::Write;

    let pass_bytes = serde_json::to_vec(pass).map_err(|_| AppError::Internal)?;
    let digest = |b: &[u8]| {
        let mut h = Sha1::new();
        h.update(b);
        hex(&h.finalize())
    };

    // Every file in the archive except the manifest and the signature itself.
    // Built once and used for BOTH the manifest and the zip, so the two can
    // never disagree — a hash for a file that is not there, or a file with no
    // hash, invalidates the signature and iOS refuses the pass without saying
    // why.
    let mut payload: Vec<(&str, &[u8])> = vec![("pass.json", pass_bytes.as_slice())];
    payload.extend(images.iter().map(|(n, b)| (n.as_str(), b.as_slice())));

    let mut manifest_map = serde_json::Map::new();
    for (name, bytes) in &payload {
        manifest_map.insert((*name).to_string(), json!(digest(bytes)));
    }
    let manifest = serde_json::to_vec(&serde_json::Value::Object(manifest_map))
        .map_err(|_| AppError::Internal)?;
    let signature = sign_manifest(&manifest)?;

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        // Stored, not deflated: a pass is three small files and iOS does not
        // care, so the simpler archive is the better one to debug.
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in payload.iter().copied().chain([
            ("manifest.json", manifest.as_slice()),
            ("signature", signature.as_slice()),
        ]) {
            zip.start_file(name, opts).map_err(|_| AppError::Internal)?;
            zip.write_all(bytes).map_err(|_| AppError::Internal)?;
        }
        zip.finish().map_err(|_| AppError::Internal)?;
    }
    Ok(buf.into_inner())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build this member's `.pkpass`, ready to serve.
///
/// One place, so the pass a customer downloads and the pass their phone fetches
/// after an update are byte-identical in structure — a device that got a
/// different shape from the two paths would show a pass that never settles.
pub async fn build_pass_for(pool: &PgPool, member: &MemberRow) -> Result<Vec<u8>, AppError> {
    let settings = crate::loyalty::settings::load_scope(pool, member.org_id, None)
        .await?
        .unwrap_or_else(|| LoyaltySettings::defaults(member.org_id, None));
    let locations = locations_for_org(pool, member.org_id).await?;
    let rewards: Vec<String> =
        crate::loyalty::settings::load_effective_rewards_org(pool, member.org_id)
            .await?
            .into_iter()
            .map(|r| format!("{} — {} {}", r.name, r.cost_amount, r.cost_currency))
            .collect();
    let org = crate::orgs::branding::load(pool, member.org_id).await?;
    let brand = pass_brand(&org);
    let pass = pass_json(member, &settings, &locations, &rewards, &brand)?;
    build_pkpass(&pass, &brand.images)
}

/// Tell every device holding this member's pass to come back for a new copy.
///
/// Apple's update model is a silent APNs push carrying no payload; the device
/// then calls the pass web service for the changed pass. Skipped when Apple is
/// not configured, so an org on Google only costs nothing here.
pub async fn notify_devices(pool: &PgPool, member: &MemberRow) -> Result<(), AppError> {
    if !is_configured() {
        return Ok(());
    }
    let tokens: Vec<(String, String)> = sqlx::query_as(
        "SELECT device_library_id, push_token FROM loyalty_pass_devices WHERE customer_id = $1",
    )
    .bind(member.id)
    .fetch_all(pool)
    .await?;
    if tokens.is_empty() {
        return Ok(());
    }
    if !super::apns::is_configured() {
        tracing::info!(
            customer_id = %member.id,
            devices = tokens.len(),
            "loyalty: pass changed but APNs is not configured — \
             the customer sees the new balance next time they open the pass"
        );
        return Ok(());
    }
    let Some(topic) = pass_type_id() else {
        return Ok(());
    };

    for (device_library_id, push_token) in tokens {
        match super::apns::push(&push_token, &topic).await {
            super::apns::PushOutcome::Delivered => {}
            // The pass is gone from that device. Dropping the registration is
            // the point of distinguishing this: otherwise we push into the void
            // on every balance change, forever.
            super::apns::PushOutcome::Unregistered => {
                let _ = sqlx::query(
                    "DELETE FROM loyalty_pass_devices \
                      WHERE device_library_id = $1 AND customer_id = $2",
                )
                .bind(&device_library_id)
                .bind(member.id)
                .execute(pool)
                .await;
            }
            super::apns::PushOutcome::Failed(why) => {
                tracing::warn!(customer_id = %member.id, error = %why, "APNs push failed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn member() -> MemberRow {
        MemberRow {
            id: uuid::Uuid::nil(),
            org_id: uuid::Uuid::nil(),
            name: "Ali Hassan".into(),
            phone: "+201000000000".into(),
            member_token: "Mabcdefghijklmnopqrstuv".into(),
            points_balance: 30,
            visits_balance: 3,
            lifetime_points: 130,
            lifetime_visits: 3,
            locale: "en".into(),
            apple_serial: None,
            apple_auth_token: Some("tok".into()),
            google_object_id: None,
            pass_updated_at: None,
            enrolled_at: chrono::Utc::now(),
        }
    }

    /// Take the wallet env lock for the duration of a test. Held by every test
    /// here, because these variables are process-global.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn configured() {
        // SAFETY: callers hold `env_guard`, so this is the only thread touching
        // the wallet environment.
        unsafe {
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.cloud.madar-pos.loyalty");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        }
    }

    #[test]
    fn balance_is_primary_and_progress_is_secondary() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[], &PassBrand::default()).unwrap();
        assert_eq!(p["storeCard"]["primaryFields"][0]["value"], 30);
        assert_eq!(
            p["storeCard"]["secondaryFields"][0]["value"],
            "30 / 100 to your next reward"
        );
        // Wallet stacks cards and shows only the header strip. Without this a
        // customer cannot see their balance without tapping the pass open.
        assert_eq!(p["storeCard"]["headerFields"][0]["value"], 30);
        assert_eq!(p["storeCard"]["headerFields"][0]["label"], "Points");
        // The auxiliary row names what they are counting FOR. It used to repeat
        // the member's name, which is already under the barcode and on the back.
        assert_eq!(
            p["storeCard"]["auxiliaryFields"][0]["value"], "100 points",
            "with no reward catalogue, the target stands in"
        );
        let with = pass_json(
            &member(),
            &s,
            &[],
            &["Free espresso — 5 visits".to_string()],
            &PassBrand::default(),
        )
        .unwrap();
        assert_eq!(
            with["storeCard"]["auxiliaryFields"][0]["value"],
            "Free espresso — 5 visits"
        );
        // And the name is still on the card, once, where a teller reads it.
        assert_eq!(p["barcodes"][0]["altText"], "Ali Hassan");
    }

    #[test]
    fn a_stamp_card_is_explained_in_stamps_not_pounds() {
        let _guard = env_guard();
        configured();
        let mut s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        let p = pass_json(&member(), &s, &[], &[], &PassBrand::default()).unwrap();
        // The stamps balance leads, not the points one.
        assert_eq!(p["storeCard"]["primaryFields"][0]["value"], 3);
        assert_eq!(p["storeCard"]["primaryFields"][0]["label"], "Orders");
        // A stamp card reads as a STEPPER, not as arithmetic: five orders is
        // few enough to count at a glance, and joining the steps shows the
        // direction of travel the way loose dots do not.
        assert_eq!(
            p["storeCard"]["secondaryFields"][0]["value"],
            "●─●─●─○─○   3 / 5"
        );
        let how = p["storeCard"]["backFields"][0]["value"].as_str().unwrap();
        assert!(how.contains("stamp"), "{how}");
        assert!(
            !how.contains("EGP"),
            "a stamp card must not talk in EGP: {how}"
        );
    }

    #[test]
    fn the_pass_wears_the_shop_and_not_apple_default_grey() {
        configured();
        let _guard = env_guard();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = PassBrand {
            org_name: "RUE Coffee".into(),
            background: "#7B1E3A".into(),
            foreground: "#EFF3F4".into(),
            label: "#C8607F".into(),
            ..PassBrand::default()
        };
        let p = pass_json(&member(), &s, &[], &[], &brand).unwrap();

        // The colours reach the pass. They used to be read from
        // `loyalty_settings`, which stopped being written when branding moved
        // to the organisation — so every pass came back grey however the card
        // looked on the web, and a fresh download looked identical to an old one.
        assert_eq!(p["backgroundColor"], "rgb(123, 30, 58)");
        assert_eq!(p["foregroundColor"], "rgb(239, 243, 244)");
        assert_eq!(p["labelColor"], "rgb(200, 96, 127)");

        // And the SHOP's name is what a customer sees in their wallet list,
        // not the programme's.
        assert_eq!(p["organizationName"], "RUE Coffee");
    }

    #[test]
    fn a_nameless_org_falls_back_to_the_programme_rather_than_blank() {
        configured();
        let _guard = env_guard();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[], &PassBrand::default()).unwrap();
        assert_eq!(p["organizationName"], s.program_name);
        // Madar's palette, not an absent one — a pass with no colours is grey.
        assert_eq!(p["backgroundColor"], "rgb(13, 98, 115)");
    }

    #[test]
    fn a_shop_logo_becomes_the_full_apple_image_set() {
        // Apple needs icon and logo at three scales each, inside the archive —
        // a pass cannot reference an image by URL the way the web card does.
        let mut img = image::RgbaImage::new(400, 120);
        for px in img.pixels_mut() {
            *px = image::Rgba([123, 30, 58, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();

        let decoded = image::load_from_memory(&png.into_inner()).unwrap();
        let set = images_from_logo(&decoded, None).expect("a real PNG resizes");
        let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "icon.png",
                "icon@2x.png",
                "icon@3x.png",
                "logo.png",
                "logo@2x.png",
                "logo@3x.png"
            ]
        );
        assert!(set.iter().all(|(_, b)| !b.is_empty()));

        // Junk falls back rather than shipping a pass with no icon, which iOS
        // refuses outright.

        // A MARK is repainted so it reads on the card. The pass's ground comes
        // from the logo's own dominant colour, so a logo left alone is very
        // nearly the colour it sits on — the shop that reported this had a blue
        // mark on a blue card.
        let mut mark = image::RgbaImage::new(40, 40);
        for (x, y, px) in mark.enumerate_pixels_mut() {
            // A shape on transparency: opaque in the middle, clear around it.
            let solid = (10..30).contains(&x) && (10..30).contains(&y);
            *px = image::Rgba([0x1E, 0x3A, 0x8A, if solid { 255 } else { 0 }]);
        }
        assert!(
            crate::orgs::branding::is_mark(&image::DynamicImage::ImageRgba8(mark.clone())),
            "a shape on transparency is a mark"
        );
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(mark)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let decoded = image::load_from_memory(&buf.into_inner()).unwrap();
        let tinted = images_from_logo(&decoded, Some("#EFF3F4")).unwrap();
        let icon = image::load_from_memory(&tinted[0].1).unwrap().to_rgba8();
        let opaque = icon.pixels().find(|p| p.0[3] > 200).expect("a shape");
        assert_eq!(
            [opaque.0[0], opaque.0[1], opaque.0[2]],
            [0xEF, 0xF3, 0xF4],
            "the mark is repainted in the pass's foreground, not left blue"
        );

        // A logo with its background BAKED IN is not a mark: every pixel is
        // opaque, so repainting it would give a solid rectangle. It keeps its
        // own colours and gets a plate to sit on instead.
        assert!(!crate::orgs::branding::is_mark(
            &image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                40,
                40,
                image::Rgba([0x1E, 0x3A, 0x8A, 255])
            ))
        ));
    }

    #[test]
    fn the_barcode_carries_the_token_not_the_id() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[], &PassBrand::default()).unwrap();
        assert_eq!(p["barcodes"][0]["message"], "Mabcdefghijklmnopqrstuv");
        assert_eq!(p["barcodes"][0]["format"], "PKBarcodeFormatQR");
        // The member id must never be the scannable value — it is guessable
        // from any other API response that carries one.
        assert_ne!(p["barcodes"][0]["message"], uuid::Uuid::nil().to_string());
    }

    #[test]
    fn branch_coordinates_become_lock_screen_locations() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let locs = vec![PassLocation {
            latitude: 30.0444,
            longitude: 31.2357,
            name: "Zamalek".into(),
        }];
        let p = pass_json(&member(), &s, &locs, &[], &PassBrand::default()).unwrap();
        assert_eq!(p["locations"][0]["latitude"], 30.0444);
        assert!(
            p["locations"][0]["relevantText"]
                .as_str()
                .unwrap()
                .contains("Zamalek")
        );
    }

    #[test]
    fn colours_are_converted_and_bad_ones_do_not_reach_the_pass() {
        assert_eq!(hex_to_rgb_css("#0D6273"), "rgb(13, 98, 115)");
        // iOS rejects a pass outright on a malformed colour, so anything
        // unparseable falls back rather than shipping.
        assert_eq!(hex_to_rgb_css("teal"), "rgb(255, 255, 255)");
    }

    #[test]
    fn a_pass_is_refused_rather_than_served_unsigned() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[], &PassBrand::default()).unwrap();
        // With no certificate configured, building an archive must fail loudly.
        // An unsigned .pkpass is rejected by iOS with no explanation at all, so
        // serving one would look to the customer like a broken link.
        let err = build_pkpass(&p, &PassBrand::default().images).unwrap_err();
        assert!(
            matches!(err, AppError::ServiceUnavailable(_)),
            "an unconfigured signer is a 503 the operator can read, not a 500"
        );
    }

    #[test]
    fn a_signed_pass_is_a_zip_of_exactly_the_three_files_ios_expects() {
        let _guard = env_guard();
        configured();
        // A throwaway self-signed cert stands in for the Pass Type ID one: it
        // exercises the real PKCS#7 path (which is where the bugs are), and iOS
        // would reject the result — as it should, since this is not Apple's.
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let key = openssl::pkey::PKey::from_rsa(rsa).unwrap();
        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "madar-test").unwrap();
        let name = name.build();
        let mut b = openssl::x509::X509::builder().unwrap();
        b.set_version(2).unwrap();
        b.set_subject_name(&name).unwrap();
        b.set_issuer_name(&name).unwrap();
        b.set_pubkey(&key).unwrap();
        b.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        b.set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        b.sign(&key, openssl::hash::MessageDigest::sha256())
            .unwrap();
        let cert = b.build();

        // SAFETY: single-threaded test process.
        unsafe {
            std::env::set_var(
                "LOYALTY_APPLE_CERT_PEM",
                String::from_utf8(cert.to_pem().unwrap()).unwrap(),
            );
            std::env::set_var(
                "LOYALTY_APPLE_KEY_PEM",
                String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap(),
            );
            std::env::set_var(
                "LOYALTY_APPLE_WWDR_PEM",
                String::from_utf8(cert.to_pem().unwrap()).unwrap(),
            );
        }

        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let pass = pass_json(&member(), &s, &[], &[], &PassBrand::default()).unwrap();
        let bytes = build_pkpass(&pass, &PassBrand::default().images).unwrap();

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        // `icon.png` is MANDATORY. A pass without one is rejected by iOS with
        // nothing but "cannot download this file" — no mention of an icon, and
        // nothing in any log. This assertion previously listed only the three
        // metadata files and so pinned that exact bug as correct.
        assert!(
            names.contains(&"icon.png".to_string()),
            "a pass without icon.png is refused by iOS: {names:?}"
        );
        assert_eq!(
            names,
            [
                "icon.png",
                "icon@2x.png",
                "icon@3x.png",
                "logo.png",
                "logo@2x.png",
                "logo@3x.png",
                "manifest.json",
                "pass.json",
                "signature",
            ]
        );

        // The manifest must carry the SHA-1 of the pass bytes actually shipped;
        // a digest of anything else is a pass iOS silently discards.
        use std::io::Read;
        let mut manifest = String::new();
        zip.by_name("manifest.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        let mut pass_bytes = Vec::new();
        zip.by_name("pass.json")
            .unwrap()
            .read_to_end(&mut pass_bytes)
            .unwrap();
        let digest = {
            use sha1::{Digest, Sha1};
            let mut h = Sha1::new();
            h.update(&pass_bytes);
            hex(&h.finalize())
        };
        let parsed: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["pass.json"], digest);

        // The manifest must cover EVERY file in the archive bar itself and the
        // signature, and hash nothing that is absent. Either mismatch
        // invalidates the signature, and iOS reports neither.
        let hashed: std::collections::BTreeSet<String> =
            parsed.as_object().unwrap().keys().cloned().collect();
        let shipped: std::collections::BTreeSet<String> = names
            .iter()
            .filter(|n| *n != "manifest.json" && *n != "signature")
            .cloned()
            .collect();
        assert_eq!(hashed, shipped, "manifest and archive must agree exactly");

        // And each hash must be of the bytes actually shipped, not of an
        // earlier version of the file.
        for name in &shipped {
            use sha1::{Digest, Sha1};
            let mut bytes = Vec::new();
            zip.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
            let mut h = Sha1::new();
            h.update(&bytes);
            assert_eq!(
                parsed[name].as_str().unwrap(),
                hex(&h.finalize()),
                "{name}: manifest hash does not match the shipped bytes"
            );
        }

        unsafe {
            std::env::remove_var("LOYALTY_APPLE_CERT_PEM");
            std::env::remove_var("LOYALTY_APPLE_KEY_PEM");
            std::env::remove_var("LOYALTY_APPLE_WWDR_PEM");
        }
    }
}
