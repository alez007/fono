// SPDX-License-Identifier: GPL-3.0-only
//! Probe: print the filtered STT/polish backend list for the loaded secrets.
//! Used to verify the "configured backends" logic without running the daemon.

use fono_core::{
    config::{LlmBackend, SttBackend},
    providers, Secrets,
};

fn main() {
    let path =
        std::env::args().nth(1).unwrap_or_else(|| "/root/.config/fono/secrets.toml".to_string());
    let secrets = Secrets::load(std::path::Path::new(&path)).expect("load secrets");
    println!("secrets file : {path}");
    println!("keys present : {:?}", secrets.keys.keys().collect::<Vec<_>>());
    let stt = providers::configured_stt_backends(&secrets, &SttBackend::Groq);
    // `false` = no self-hosted server URL configured, so the `network`
    // row stays hidden; pass `true` to see it listed.
    let llm = providers::configured_llm_backends(&secrets, &LlmBackend::Groq, false);
    println!("STT visible  : {stt:?}");
    println!("LLM visible  : {llm:?}");
}
