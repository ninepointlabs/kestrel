//! End-to-end checks of the built binary against a throwaway config dir.
//! `daily_limit = 0` keeps every post blocked, so no request ever reaches X.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_kestrel");

fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kestrel-smoke-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn kestrel(home: &PathBuf, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn configure(home: &PathBuf, limit: u32) {
    let out = kestrel(
        home,
        &["configure"],
        &format!("k1234\ns5678\nt9012\nts3456\n{limit}\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn missing_config_gives_guidance() {
    let home = temp_home("noconfig");
    let out = kestrel(&home, &["status"], "");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("kestrel configure"), "{err}");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn configure_status_and_limit_refusal() {
    let home = temp_home("cli");
    configure(&home, 0);

    let path = home.join(".config/kestrel/config.toml");
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("api_key = \"k1234\""), "{saved}");
    assert!(saved.contains("daily_limit = 0"), "{saved}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let out = kestrel(&home, &["status"], "");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Posts today: 0/0 (100%)"
    );

    let out = kestrel(&home, &["post", "hello"], "");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("daily post limit reached"), "{err}");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn mcp_server_lists_tools_and_enforces_limit() {
    let home = temp_home("mcp");
    configure(&home, 0);

    let session = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"kestrel_status","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"kestrel_post","arguments":{"text":"hi"}}}"#,
    ];

    let mut child = Command::new(BIN)
        .arg("serve")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for line in session {
        writeln!(stdin, "{line}").unwrap();
        // Give the server time to answer in order before the next message.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    drop(stdin); // EOF shuts the server down
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("MCP transcript:\n{stdout}");

    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no response for id {id}"))
    };

    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "kestrel");

    let tools = by_id(2)["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"kestrel_post") && names.contains(&"kestrel_status"));
    let post = tools.iter().find(|t| t["name"] == "kestrel_post").unwrap();
    assert_eq!(post["inputSchema"]["required"], serde_json::json!(["text"]));

    let status_text = by_id(3)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(status_text.contains("Posts today: 0/0"), "{status_text}");

    let post_result = &by_id(4)["result"];
    assert_eq!(post_result["isError"], true);
    let post_text = post_result["content"][0]["text"].as_str().unwrap();
    assert!(
        post_text.contains("daily post limit reached"),
        "{post_text}"
    );

    let _ = std::fs::remove_dir_all(home);
}
