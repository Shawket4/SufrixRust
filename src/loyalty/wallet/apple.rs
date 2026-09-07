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
use uuid::Uuid;

use crate::errors::AppError;
use crate::loyalty::model::MemberRow;
use crate::loyalty::settings::LoyaltySettings;

use super::google::progress_line;

/// Apple shows at most ten locations on a pass; more are ignored, so sending
/// more wastes bytes on every device that holds it.
const MAX_LOCATIONS: usize = 10;

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Read PEM material from `KEY_FILE` (a path) or `KEY` (inline).
///
/// The file form is the one to use in production: a private key in an env var
/// has to have its newlines escaped, shows up in `docker inspect`, and lands in
/// any process listing that dumps the environment. The inline form stays for
/// local development and tests.
fn pem_material(key: &str) -> Option<Vec<u8>> {
    if let Some(path) = env_nonempty(&format!("{key}_FILE")) {
        return match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                // A configured-but-unreadable key is an operator error worth
                // shouting about — silently falling back to "no Apple wallet"
                // would look like it was never configured.
                tracing::error!(path = %path, error = %e, "cannot read {key}_FILE");
                None
            }
        };
    }
    env_nonempty(key).map(|s| s.replace("\\n", "\n").into_bytes())
}

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

/// A branch as the pass surfaces it on the lock screen.
#[derive(Debug, Clone)]
pub struct PassLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub name: String,
}

/// Every branch of the org that has coordinates.
///
/// These are the columns the staff-geofencing work already added and the branch
/// dialog already edits — the program needed no new location UI, only a reason
/// to read them.
pub async fn locations_for_org(pool: &PgPool, org_id: Uuid) -> Result<Vec<PassLocation>, AppError> {
    let rows: Vec<(f64, f64, String)> = sqlx::query_as(
        "SELECT latitude, longitude, name FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL \
            AND latitude IS NOT NULL AND longitude IS NOT NULL \
          ORDER BY name LIMIT $2",
    )
    .bind(org_id)
    .bind(MAX_LOCATIONS as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(latitude, longitude, name)| PassLocation {
            latitude,
            longitude,
            name,
        })
        .collect())
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
        "organizationName": program,
        "description": format!("{program} card"),
        // The web service that serves updates. Apple only calls it when the
        // pass carries an auth token, which is minted at signup.
        "authenticationToken": member.apple_auth_token,
        "storeCard": {
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
            "auxiliaryFields": [{
                "key": "name",
                "label": "Member",
                "value": member.name
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

    if let Some(base) = super::loyalty_base() {
        // Apple appends `/v1/...` itself, so this is the scope's parent.
        pass["webServiceURL"] = json!(format!("{base}/api/wallet"));
    }
    if let Some(c) = &settings.pass_background_color {
        pass["backgroundColor"] = json!(hex_to_rgb_css(c));
    }
    if let Some(c) = &settings.pass_foreground_color {
        pass["foregroundColor"] = json!(hex_to_rgb_css(c));
    }
    if let Some(c) = &settings.pass_label_color {
        pass["labelColor"] = json!(hex_to_rgb_css(c));
    }
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
    let cert_pem = pem_material("LOYALTY_APPLE_CERT_PEM").ok_or_else(|| missing("the certificate"))?;
    let key_pem = pem_material("LOYALTY_APPLE_KEY_PEM").ok_or_else(|| missing("the private key"))?;
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
pub fn build_pkpass(pass: &serde_json::Value) -> Result<Vec<u8>, AppError> {
    use sha1::{Digest, Sha1};
    use std::io::Write;

    let pass_bytes = serde_json::to_vec(pass).map_err(|_| AppError::Internal)?;
    let digest = |b: &[u8]| {
        let mut h = Sha1::new();
        h.update(b);
        hex(&h.finalize())
    };
    let manifest = serde_json::to_vec(&json!({ "pass.json": digest(&pass_bytes) }))
        .map_err(|_| AppError::Internal)?;
    let signature = sign_manifest(&manifest)?;

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        // Stored, not deflated: a pass is three small files and iOS does not
        // care, so the simpler archive is the better one to debug.
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in [
            ("pass.json", pass_bytes.as_slice()),
            ("manifest.json", manifest.as_slice()),
            ("signature", signature.as_slice()),
        ] {
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
    let pass = pass_json(member, &settings, &locations, &rewards)?;
    build_pkpass(&pass)
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
mod tests {
    use super::*;

    fn member() -> MemberRow {
        MemberRow {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
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

    fn configured() {
        // SAFETY: single-threaded test process; these are read, never mutated,
        // by the code under test.
        unsafe {
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.cloud.madar-pos.loyalty");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        }
    }

    #[test]
    fn balance_is_primary_and_progress_is_secondary() {
        configured();
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[]).unwrap();
        assert_eq!(p["storeCard"]["primaryFields"][0]["value"], 30);
        assert_eq!(
            p["storeCard"]["secondaryFields"][0]["value"],
            "30 / 100 to your next reward"
        );
        assert_eq!(p["storeCard"]["auxiliaryFields"][0]["value"], "Ali Hassan");
    }

    #[test]
    fn a_stamp_card_is_explained_in_stamps_not_pounds() {
        configured();
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        let p = pass_json(&member(), &s, &[], &[]).unwrap();
        // The stamps balance leads, not the points one.
        assert_eq!(p["storeCard"]["primaryFields"][0]["value"], 3);
        assert_eq!(p["storeCard"]["primaryFields"][0]["label"], "Orders");
        assert_eq!(
            p["storeCard"]["secondaryFields"][0]["value"],
            "3 / 5 to your next reward"
        );
        let how = p["storeCard"]["backFields"][0]["value"].as_str().unwrap();
        assert!(how.contains("stamp"), "{how}");
        assert!(
            !how.contains("EGP"),
            "a stamp card must not talk in EGP: {how}"
        );
    }

    #[test]
    fn the_barcode_carries_the_token_not_the_id() {
        configured();
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[]).unwrap();
        assert_eq!(p["barcodes"][0]["message"], "Mabcdefghijklmnopqrstuv");
        assert_eq!(p["barcodes"][0]["format"], "PKBarcodeFormatQR");
        // The member id must never be the scannable value — it is guessable
        // from any other API response that carries one.
        assert_ne!(p["barcodes"][0]["message"], Uuid::nil().to_string());
    }

    #[test]
    fn branch_coordinates_become_lock_screen_locations() {
        configured();
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let locs = vec![PassLocation {
            latitude: 30.0444,
            longitude: 31.2357,
            name: "Zamalek".into(),
        }];
        let p = pass_json(&member(), &s, &locs, &[]).unwrap();
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
        configured();
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let p = pass_json(&member(), &s, &[], &[]).unwrap();
        // With no certificate configured, building an archive must fail loudly.
        // An unsigned .pkpass is rejected by iOS with no explanation at all, so
        // serving one would look to the customer like a broken link.
        let err = build_pkpass(&p).unwrap_err();
        assert!(
            matches!(err, AppError::ServiceUnavailable(_)),
            "an unconfigured signer is a 503 the operator can read, not a 500"
        );
    }

    #[test]
    fn a_signed_pass_is_a_zip_of_exactly_the_three_files_ios_expects() {
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
        b.sign(&key, openssl::hash::MessageDigest::sha256()).unwrap();
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

        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let pass = pass_json(&member(), &s, &[], &[]).unwrap();
        let bytes = build_pkpass(&pass).unwrap();

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["manifest.json", "pass.json", "signature"]);

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

        unsafe {
            std::env::remove_var("LOYALTY_APPLE_CERT_PEM");
            std::env::remove_var("LOYALTY_APPLE_KEY_PEM");
            std::env::remove_var("LOYALTY_APPLE_WWDR_PEM");
        }
    }
}
