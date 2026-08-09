//! filex-sign — release signing helper (CI + one-time keygen).
//!
//! Two subcommands, both built without the GPUI app (`--no-default-features`):
//!
//! * `filex-sign keygen`
//!     Prints a fresh Ed25519 keypair. **Run once, locally.** Embed the
//!     public key in the shipping binaries (the `UPDATE_PUBLIC_KEY` /
//!     manifest-URL constants) and store the private key as the CI secret
//!     `FILEX_SIGNING_KEY`. The private key must never enter the repo.
//!
//! * `filex-sign sign --version <v> --url <artifact-url> --in <file> \
//!                    --out <manifest.json> [--key <hex>]`
//!     Computes the artifact's SHA-256 and Ed25519 signature (using the
//!     same `filex::update` code the client verifies with) and writes the
//!     update manifest JSON. The private key is read from `--key` or, if
//!     omitted, the `FILEX_SIGNING_KEY` env var — so CI never puts it on
//!     the command line.
//!
//! This binary has no network or platform dependencies, so it runs on
//! every release runner (Windows / macOS / Linux) identically.

use std::process::ExitCode;

use filex::update::{self, Manifest};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("keygen") => keygen(),
        Some("sign") => match sign(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(msg) => fail(&msg),
        },
        other => {
            eprintln!("filex-sign: unknown command {other:?}");
            eprintln!("usage: filex-sign keygen");
            eprintln!(
                "       filex-sign sign --version <v> --url <url> --in <file> \
                 --out <manifest.json> [--key <hex>]"
            );
            ExitCode::FAILURE
        }
    }
}

fn keygen() -> ExitCode {
    match update::generate_keypair() {
        Ok((public_hex, private_hex)) => {
            // Public key to stdout (safe to embed / log); private key clearly
            // labelled so it isn't pasted anywhere public.
            println!("public_key  {public_hex}");
            println!("private_key {private_hex}");
            eprintln!(
                "\nEmbed public_key in the binaries; store private_key as the \
                 CI secret FILEX_SIGNING_KEY. Do NOT commit the private key."
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}

fn sign(args: impl Iterator<Item = String>) -> Result<(), String> {
    let opts = parse_flags(args)?;
    let get = |name: &str| opts.get(name).cloned().ok_or(format!("missing --{name}"));

    let version = get("version")?;
    let url = get("url")?;
    let input = get("in")?;
    let output = get("out")?;
    let key = match opts.get("key") {
        Some(k) => k.clone(),
        None => std::env::var("FILEX_SIGNING_KEY")
            .map_err(|_| "no --key and FILEX_SIGNING_KEY is unset".to_string())?,
    };

    let data = std::fs::read(&input).map_err(|e| format!("reading {input}: {e}"))?;
    let (sha256, signature) =
        update::sign_artifact(&key, &data).map_err(|e| format!("signing: {e}"))?;

    let manifest = Manifest {
        version,
        url,
        sha256,
        signature,
        minimum_version: None,
    };
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| format!("serializing manifest: {e}"))?;
    std::fs::write(&output, json).map_err(|e| format!("writing {output}: {e}"))?;
    eprintln!(
        "wrote {output} for {} ({} bytes)",
        manifest.version,
        data.len()
    );
    Ok(())
}

/// Parse `--flag value` pairs into a map. Rejects a flag with no value.
fn parse_flags(
    mut args: impl Iterator<Item = String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut map = std::collections::HashMap::new();
    while let Some(arg) = args.next() {
        let Some(name) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        let value = args.next().ok_or(format!("--{name} needs a value"))?;
        map.insert(name.to_string(), value);
    }
    Ok(map)
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("filex-sign: {msg}");
    ExitCode::FAILURE
}
