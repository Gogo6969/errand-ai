//! Actually asking a model something.
//!
//! One function, `ask`, which takes a role rather than a model. Callers say
//! what kind of question it is and this works out where to send it, falls back
//! when something is down, and reports which model answered so a run can say so
//! rather than leaving you guessing.

use errand_core::providers::{Kind, Provider, Role};

use crate::state::AppState;

/// What answered, and what it said.
#[derive(Debug, Clone)]
pub struct Answer {
    pub text: String,
    /// Shown in the run, so you can tell what actually did the thinking.
    pub provider_label: String,
    pub model: String,
    /// True when nothing left this machine.
    pub was_local: bool,
}

/// Ask the model bound to a role.
///
/// Walks the chain in order, so a local model being switched off falls through
/// to the next rather than failing the run.
pub async fn ask(state: &AppState, role: Role, prompt: &str) -> anyhow::Result<Answer> {
    let providers = errand_core::db::list_providers(state.pool()).await?;
    let bindings = errand_core::db::list_role_bindings(state.pool()).await?;
    let local_only = errand_core::db::get_setting(state.pool(), "privacy.local_only")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let chain = errand_core::providers::resolve_chain(&providers, &bindings, role, local_only);
    if chain.is_empty() {
        anyhow::bail!(errand_core::providers::explain_empty_chain(
            role, local_only, &providers
        ));
    }

    let mut last: Option<String> = None;
    for p in chain {
        let model = p
            .model
            .clone()
            .unwrap_or_else(|| errand_core::providers::default_model_for(role).to_string());

        let result = match p.kind_enum() {
            Some(Kind::ClaudeCli) => crate::executor::ask_model(prompt, &model, 4)
                .await
                .map_err(|e| e.to_string()),
            Some(Kind::OpenAiCompat) => {
                let key = key_for(&p.id).await;
                ask_openai_compatible(
                    p.base_url.as_deref().unwrap_or(""),
                    &model,
                    prompt,
                    key.as_deref(),
                )
                .await
            }
            Some(Kind::AnthropicApi) => ask_anthropic(&model, prompt).await,
            None => Err("that provider has an unknown type".into()),
        };

        match result {
            Ok(text) if !text.trim().is_empty() => {
                return Ok(Answer {
                    text,
                    provider_label: p.label.clone(),
                    model,
                    was_local: p.is_local(),
                })
            }
            Ok(_) => {
                last = Some(format!("{} returned nothing", p.label));
            }
            Err(e) => {
                tracing::warn!(provider = %p.label, "could not use this model: {e}");
                last = Some(format!("{}: {e}", p.label));
            }
        }
    }

    anyhow::bail!(
        "None of the models Errand could reach answered. Last problem: {}",
        last.unwrap_or_else(|| "unknown".into())
    )
}

/// Anything speaking the OpenAI chat format.
///
/// This is the whole point of the module: OpenAI, Google, OpenRouter, xAI,
/// DeepSeek, Moonshot, Mistral, Groq, Z.ai, Together, Fireworks, Cerebras,
/// Perplexity, Ollama, LM Studio, vLLM and llama.cpp all speak it, so one
/// adapter reaches every one of them and adding another is a row in a list
/// rather than a new code path.
///
/// The key is passed in rather than looked up here, so this function never
/// touches the keychain and can be tested against a stub without one.
pub async fn ask_openai_compatible(
    base_url: &str,
    model: &str,
    prompt: &str,
    key: Option<&str>,
) -> Result<String, String> {
    if base_url.is_empty() {
        return Err("no address configured".into());
    }
    let mut req = reqwest::Client::new().post(format!(
        "{}/chat/completions",
        base_url.trim_end_matches('/')
    ));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let res = req
        .json(&serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": prompt }],
            "temperature": 0.2,
            "stream": false,
        }))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("could not reach it: {e}"))?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        // The two that are worth naming, because the fix is different and
        // "it returned 401" tells nobody which one it was.
        return Err(match status.as_u16() {
            401 | 403 => {
                "it refused the key. Check the key, and that it is for this service.".to_string()
            }
            429 => "it is rate limiting or out of credit.".to_string(),
            _ => format!("it returned {status}"),
        });
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "it returned something unreadable".to_string())?;
    // Some services put a refusal in a 200. Better to say so than to return an
    // empty answer that reads as a model having nothing to add.
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(msg.to_string());
    }
    Ok(v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// Anthropic's API, with a key from the keychain.
///
/// The key is read at the moment of use and never stored anywhere else, like
/// every other secret in this program.
async fn ask_anthropic(model: &str, prompt: &str) -> Result<String, String> {
    let key = crate::secrets::get_internal("anthropic.api_key")
        .await
        .map_err(|_| "no Anthropic key is saved. Add one in Settings.".to_string())?;

    let res = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key.expose())
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": model,
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": prompt }],
        }))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("could not reach Anthropic: {e}"))?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status.as_u16() == 401 {
        return Err("Anthropic refused that key.".into());
    }
    if !status.is_success() {
        return Err(format!("Anthropic returned {status}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "unreadable reply".to_string())?;
    Ok(v["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// Ask an endpoint what it can run.
pub async fn list_models(base_url: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let mut req = reqwest::Client::new().get(format!("{}/models", base_url.trim_end_matches('/')));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let res = req
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    if !res.status().is_success() {
        return Err(match res.status().as_u16() {
            401 | 403 => "it refused the key".to_string(),
            other => format!("it returned {other}"),
        });
    }
    let v: serde_json::Value = res.json().await.map_err(|e| format!("{e}"))?;
    Ok(v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// This provider's key, if one was saved.
///
/// Read at the moment of use and dropped straight afterwards, so a key is never
/// held in memory between runs and never reaches the database or a log.
pub async fn key_for(provider_id: &str) -> Option<String> {
    crate::secrets::get_internal(&errand_core::providers::key_account(provider_id))
        .await
        .ok()
        .map(|s| s.expose().to_string())
}

/// The Claude command line tool, which Errand sets up for itself.
pub const BUILTIN_CLAUDE: &str = "builtin-claude-cli";

/// Make sure there is always something in the list.
///
/// A brand new install has an empty Settings screen otherwise, which reads as
/// "this is broken" rather than "this uses Claude by default". The row is added
/// once and then belongs to the person: switching it off keeps it off.
pub async fn ensure_builtin(pool: &errand_core::db::Pool) -> anyhow::Result<()> {
    let existing = errand_core::db::list_providers(pool).await?;
    if existing.iter().any(|p| p.id == BUILTIN_CLAUDE) {
        return Ok(());
    }
    errand_core::db::upsert_provider(
        pool,
        &Provider {
            id: BUILTIN_CLAUDE.into(),
            kind: Kind::ClaudeCli.as_str().into(),
            label: "Claude (command line tool)".into(),
            base_url: None,
            model: None,
            enabled: true,
            discovered: true,
            health: None,
            health_detail: None,
        },
    )
    .await?;
    Ok(())
}

/// Check each provider and write down what was found, so the Settings screen
/// can say "not answering" instead of showing a hopeful green dot.
pub async fn refresh_health(pool: &errand_core::db::Pool) -> anyhow::Result<()> {
    for p in errand_core::db::list_providers(pool).await? {
        let (status, detail) = check_one(&p).await;
        errand_core::db::set_provider_health(pool, &p.id, status, Some(&detail)).await?;
    }
    Ok(())
}

/// Is this provider actually usable right now?
pub async fn check_one(p: &Provider) -> (&'static str, String) {
    match p.kind_enum() {
        Some(Kind::ClaudeCli) => match crate::executor::find_claude() {
            Some(path) => ("ok", format!("found at {}", path.display())),
            None => (
                "missing",
                "Not installed, or not on the path this background service can see. Install the \
                 Claude command line tool and run 'claude /login' once."
                    .into(),
            ),
        },
        Some(Kind::AnthropicApi) => match crate::secrets::get_internal("anthropic.api_key").await {
            Ok(_) => ("ok", "a key is saved in your keychain".into()),
            Err(_) => ("missing", "No key saved yet.".into()),
        },
        Some(Kind::OpenAiCompat) => {
            let base = p.base_url.clone().unwrap_or_default();
            let key = key_for(&p.id).await;
            if key.is_none() && !p.is_local() {
                return (
                    "missing",
                    "This service needs a key, and none is saved. Add one and check again.".into(),
                );
            }
            match list_models(&base, key.as_deref()).await {
                Ok(m) if m.is_empty() => (
                    "empty",
                    "It answered, but has no models loaded. Load one and check again.".into(),
                ),
                Ok(m) => ("ok", format!("{} model(s): {}", m.len(), m.join(", "))),
                Err(e) => ("unreachable", format!("Nothing answered at {base}. {e}")),
            }
        }
        None => ("unknown", "Errand does not recognise this kind.".into()),
    }
}

/// Look for models, on this machine and optionally on this network.
///
/// Two quite different things, which is why they are separate. Loopback is
/// always safe to try: it is your own computer and nobody else can see it.
/// Scanning the network is not something to do behind someone's back — on an
/// office or hotel network it is rude, and it looks like something worse — so
/// it happens only when asked for, and only across the subnet this machine is
/// already on.
///
/// Nothing found is switched on. Finding a model and trusting it are separate
/// decisions, and the second one is not Errand's to make.
pub async fn discover(scan_network: bool) -> Vec<Provider> {
    let mut hosts: Vec<String> = vec!["127.0.0.1".into()];
    if scan_network {
        hosts.extend(subnet_hosts());
    }

    // The ports worth trying on another machine. Loopback tries everything;
    // across a network the long tail is mostly other people's web servers.
    let lan_ports = [11434u16, 1234, 3000, 8080, 8000];

    let mut jobs = vec![];
    for host in &hosts {
        let local = host == "127.0.0.1";
        for probe in errand_core::providers::PROBES {
            if !local && !lan_ports.contains(&probe.port) {
                continue;
            }
            jobs.push((host.clone(), probe.port, probe.what));
        }
    }

    tracing::info!(
        addresses = hosts.len(),
        probes = jobs.len(),
        scan_network,
        "looking for models"
    );

    // Bounded, so a /24 does not open two thousand sockets at once and get the
    // machine throttled by its own network stack.
    let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
    let mut tasks = tokio::task::JoinSet::new();
    for (host, port, what) in jobs {
        let limit = limit.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await.ok()?;

            // A plain connection first. Almost everything scanned is nothing at
            // all, and a refused connection costs a millisecond where an HTTP
            // request to a dead address costs the whole timeout.
            let addr = format!("{host}:{port}");
            let connected = tokio::time::timeout(
                std::time::Duration::from_millis(400),
                tokio::net::TcpStream::connect(&addr),
            )
            .await;
            if !matches!(connected, Ok(Ok(_))) {
                return None;
            }

            let base = format!("http://{host}:{port}/v1");
            let models = tokio::time::timeout(
                std::time::Duration::from_millis(2500),
                list_models(&base, None),
            )
            .await
            .ok()?
            .ok()?;
            if models.is_empty() {
                return None;
            }

            Some(Provider {
                id: format!("found:{base}"),
                kind: Kind::OpenAiCompat.as_str().into(),
                label: if host == "127.0.0.1" {
                    format!("{what} on this machine")
                } else {
                    format!("{what} on {host}")
                },
                base_url: Some(base),
                model: models.first().cloned(),
                enabled: false,
                discovered: true,
                health: Some("ok".into()),
                health_detail: Some(format!(
                    "{} model(s): {}",
                    models.len(),
                    models
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            })
        });
    }

    let mut found = vec![];
    while let Some(res) = tasks.join_next().await {
        if let Ok(Some(p)) = res {
            found.push(p);
        }
    }
    // A stable order, so the same scan twice does not reshuffle the list.
    found.sort_by(|a, b| a.base_url.cmp(&b.base_url));
    found
}

/// Every other address on the networks this machine is already on.
///
/// Only a /24, and only for interfaces that actually have a private address.
/// A machine on a large corporate network is not going to be swept, both
/// because it would take all day and because it is not Errand's business.
fn subnet_hosts() -> Vec<String> {
    let mut hosts = vec![];
    for ip in own_addresses() {
        let o = ip.octets();
        if !ip.is_private() {
            continue;
        }
        for last in 1u16..=254 {
            let candidate = format!("{}.{}.{}.{}", o[0], o[1], o[2], last);
            // Skip ourselves: loopback covers this machine already.
            if last as u8 != o[3] {
                hosts.push(candidate);
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// This machine's own IPv4 addresses.
fn own_addresses() -> Vec<std::net::Ipv4Addr> {
    let Ok(out) = std::process::Command::new("/sbin/ifconfig")
        .arg("-a")
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found = vec![];
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("inet ") else {
            continue;
        };
        let Some(addr) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() {
            if ip.is_private() {
                found.push(ip);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_network_is_never_scanned_unless_it_was_asked_for() {
        // Sweeping somebody's network unasked is rude, and on a work network it
        // looks like something else entirely. The default must stay loopback.
        let found = discover(false).await;
        for p in &found {
            let url = p.base_url.clone().unwrap_or_default();
            assert!(
                url.starts_with("http://127.0.0.1:"),
                "an unasked-for scan reached {url}"
            );
        }
    }

    #[test]
    fn a_sweep_covers_the_subnet_and_leaves_out_this_machine() {
        let hosts = subnet_hosts();
        for h in &hosts {
            let ip: std::net::Ipv4Addr = h.parse().expect("every candidate is an address");
            assert!(
                ip.is_private(),
                "a sweep must stay on a private network: {h}"
            );
        }
        // Either this machine has no private address, or the sweep is a /24
        // with this machine itself left out.
        if !hosts.is_empty() {
            assert!(
                hosts.len().is_multiple_of(253),
                "a sweep is a /24 minus ourselves"
            );
            for own in own_addresses() {
                assert!(
                    !hosts.contains(&own.to_string()),
                    "the sweep should not include this machine"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_endpoint_that_is_not_there_fails_quickly_and_clearly() {
        // Port 9 discards everything, so nothing will answer.
        let e = ask_openai_compatible("http://127.0.0.1:9", "any", "hello", None)
            .await
            .unwrap_err();
        assert!(e.contains("could not reach it"), "unhelpful: {e}");
    }

    #[tokio::test]
    async fn an_empty_address_is_refused_rather_than_requested() {
        assert!(ask_openai_compatible("", "m", "p", None).await.is_err());
    }

    /// A stand-in for Ollama. Raw TCP rather than a web framework, because the
    /// point is to prove Errand speaks the wire format, not to test a library.
    async fn stub(reply: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else {
                    return;
                };
                let body = reply.to_string();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let res = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(res.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_local_model_really_answers() {
        let base =
            stub(r#"{"choices":[{"message":{"content":"CAUSE: the site was down"}}]}"#).await;
        let got = ask_openai_compatible(&base, "llama3.1", "why did it fail?", None)
            .await
            .unwrap();
        assert_eq!(got, "CAUSE: the site was down");
    }

    #[tokio::test]
    async fn a_reply_in_a_shape_errand_does_not_know_is_an_error_not_a_shrug() {
        // An endpoint answering 200 with something else must not be read as an
        // empty answer, or a run would act on nothing and call it a diagnosis.
        let base = stub(r#"{"unexpected":true}"#).await;
        let got = ask_openai_compatible(&base, "m", "p", None).await.unwrap();
        assert!(
            got.is_empty(),
            "unknown shapes yield nothing, and ask() treats that as a failure"
        );
    }

    #[tokio::test]
    async fn when_the_first_choice_is_dead_the_question_is_still_answered() {
        // The whole reason for a chain. Somebody points the Fixer at the model
        // on their desk, the machine is asleep, and a run must not die of it.
        let pool = errand_core::db::open_memory().await.unwrap();
        let alive = stub(r#"{"choices":[{"message":{"content":"the other one answered"}}]}"#).await;

        for (id, label, url) in [
            ("dead", "Desk machine", "http://127.0.0.1:9"),
            ("alive", "Spare", alive.as_str()),
        ] {
            errand_core::db::upsert_provider(
                &pool,
                &Provider {
                    id: id.into(),
                    kind: Kind::OpenAiCompat.as_str().into(),
                    label: label.into(),
                    base_url: Some(url.into()),
                    model: Some("m".into()),
                    enabled: true,
                    discovered: false,
                    health: None,
                    health_detail: None,
                },
            )
            .await
            .unwrap();
        }
        errand_core::db::set_role_binding(&pool, Role::Fixer, Some("dead"))
            .await
            .unwrap();

        let state = AppState::new(pool);
        let answer = ask(&state, Role::Fixer, "why?").await.unwrap();

        assert_eq!(answer.text, "the other one answered");
        assert_eq!(answer.provider_label, "Spare");
        assert!(answer.was_local);
    }

    #[tokio::test]
    async fn with_nowhere_to_send_it_the_error_says_what_to_do() {
        let pool = errand_core::db::open_memory().await.unwrap();
        let state = AppState::new(pool);
        let e = ask(&state, Role::Narrator, "hello")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("claude /login"),
            "an empty setup should say how to fix it, got: {e}"
        );
    }
}
