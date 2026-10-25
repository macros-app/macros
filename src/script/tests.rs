use std::collections::HashMap;

use futures::channel::mpsc::UnboundedReceiver;
use ropey::Rope;

use super::*;

/// A unique, empty persistence dir for one test engine. Tests must NOT share a
/// stub path (even an "unwritable" one like /nonexistent-…): CI runs as root,
/// where the shared path silently becomes real, so one test's
/// `perspective-save` leaks perspectives into every later test's
/// `perspective-load`. Unique per call; pre-cleaned so reruns start empty.
fn fresh_persistence_dir() -> String {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("macros-test-persist-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_string()
}

/// Load bundled `scheme/<name>.scm` into `engine` after everything its
/// `;; requires:` header pulls in (transitively, in startup order), so a test
/// needn't hand-list the dependency graph. Returns the module's own result
/// (its emitted commands), like evaluating the file directly.
fn load_with_deps(engine: &ScriptEngine, name: &str) -> EvalResult {
    let mut last = None;
    for src in crate::runtime::script_with_deps(name) {
        last = Some(
            engine
                .eval(src)
                .unwrap_or_else(|e| panic!("loading {name} and its deps: {e}")),
        );
    }
    last.expect("script_with_deps always ends with the script itself")
}

/// Each bundled script loads with ONLY its declared dependencies — a fresh
/// engine per file, so a missing `;; requires:` entry fails here even when the
/// real startup order happens to load the dependency first.
#[test]
fn each_startup_script_loads_with_only_declared_deps() {
    for script in crate::runtime::bundled_scripts() {
        assert!(
            crate::runtime::parse_requires(script.source).is_some(),
            "scheme/{}.scm has no `;; requires:` header (use `;; requires:` for none)",
            script.name
        );
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::script_with_deps(script.name) {
            if let Err(e) = engine.eval(src) {
                panic!(
                    "scheme/{}.scm failed to load with only its declared deps \
                     (missing a `;; requires:` entry?): {e}",
                    script.name
                );
            }
        }
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn diff_line_marks_classifies_add_modify_delete() {
        use crate::editor::DiffMark::*;
        // Added lines at the end.
        let m = diff_line_marks("a\nb\n", "a\nb\nc\nd\n");
        assert_eq!(m, vec![(2, Added), (3, Added)]);

        // A modified middle line.
        let m = diff_line_marks("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(m, vec![(1, Modified)]);

        // A deletion rides the line that now follows the removed text.
        let m = diff_line_marks("a\nb\nc\nd\n", "a\nd\n");
        assert_eq!(m, vec![(1, Removed)]);

        // A deletion at end-of-file clamps to the last remaining line.
        let m = diff_line_marks("a\nb\nc\n", "a\n");
        assert_eq!(m, vec![(0, Removed)]);

        // Untracked (empty HEAD) → every line added; identical → nothing.
        assert_eq!(diff_line_marks("", "x\ny\n"), vec![(0, Added), (1, Added)]);
        assert!(diff_line_marks("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn engine_recovers_after_error() {
        let (engine, _rx) = ScriptEngine::new();
        // Define a function, then call a NONEXISTENT one (free-identifier
        // error) — mirrors the visual-mode eval-region repro.
        let _ = engine.eval("(define (foobar a b) (+ a b))");
        let err = engine.eval("(message (foobarx 10 10))");
        assert!(err.is_err(), "expected free-identifier error");
        // After the error, the SAME engine must still evaluate cleanly.
        let ok = engine.eval("(foobar 10 10)");
        let value = ok.ok().and_then(|r| r.value);
        assert_eq!(value.as_deref(), Some("20"), "engine wedged after error");
    }

    #[test]
    fn crypto_primitives_compose_pkce_challenge() {
        let (engine, _rx) = ScriptEngine::new();
        // base64url of a plain string (no padding).
        let b64 = engine.eval(r#"(base64url-encode "abc")"#).unwrap();
        assert!(
            b64.value.as_deref().unwrap().contains("YWJj"),
            "base64url-encode wrong: {:?}",
            b64.value
        );
        // RFC 7636 Appendix B vector: challenge = base64url(sha256(verifier)).
        // `(hex->base64url (sha256 verifier))` must reproduce it exactly.
        let challenge = engine
            .eval(r#"(hex->base64url (sha256 "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"))"#)
            .unwrap();
        assert!(
            challenge
                .value
                .as_deref()
                .unwrap()
                .contains("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
            "PKCE challenge wrong: {:?}",
            challenge.value
        );
        // random-bytes returns hex: 32 bytes -> 64 hex chars.
        let rnd = engine.eval("(string-length (random-bytes 32))").unwrap();
        assert_eq!(rnd.value.as_deref(), Some("64"));
    }

    #[test]
    fn out_of_range_ports_and_sizes_raise_instead_of_wrapping() {
        let (engine, _rx) = ScriptEngine::new();
        // 70000 used to truncate to port 4464 and -1 to 65535; both now raise.
        for src in [
            r#"(tcp-listen "127.0.0.1" 70000)"#,
            r#"(tcp-listen "127.0.0.1" -1)"#,
            r#"(tcp-connect "127.0.0.1" 65536 10 10)"#,
            r#"(tcp-connect "127.0.0.1" 1 -5 10)"#,
            r#"(http-serve "127.0.0.1" 99999 "text/plain" "" 'cb)"#,
            "(http-server-stop -1)",
            "(random-bytes -1)",
            "(udp-recv 1 -1)",
        ] {
            assert!(engine.eval(src).is_err(), "{src} should raise");
        }
        // In range still works.
        let ok = engine
            .eval(r#"(tcp-listener-close (tcp-listen "127.0.0.1" 0))"#)
            .unwrap();
        assert_eq!(ok.value.as_deref(), Some("#true"));
    }

    #[test]
    fn tls_connect_reports_failure_cleanly() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // Nothing listens on port 1 — connect must return id 0, not hang or
        // panic, and leave a readable message in tls-last-error.
        assert!(val(r#"(tls-connect "127.0.0.1" 1 500 500)"#).contains('0'));
        assert!(val("(string-length (tls-last-error))") != "0");
        // Operations on a bogus id fail soft.
        assert!(
            val("(tls-send 9999 \"x\")").contains("#f")
                || val("(tls-send 9999 \"x\")").contains("false")
        );
        assert_eq!(val("(string-length (tls-recv-line 9999))"), "0");
    }

    #[test]
    fn tcp_listen_accept_roundtrips_over_loopback() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // A background thread connects to a listener the primitives create, so
        // accept → recv-line → send exercises the whole plain-TCP path with no
        // network. bind port 0 → the OS assigns; read it back.
        engine
            .eval(r#"(define srv (tcp-listen "127.0.0.1" 0))"#)
            .unwrap();
        assert!(
            val("(> srv 0)").contains("#t"),
            "listen failed: {}",
            val("(socket-last-error)")
        );
        let port: u16 = val("(tcp-listener-port srv)").parse().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"ping\n").unwrap();
            let mut line = String::new();
            BufReader::new(&c).read_line(&mut line).unwrap();
            line
        });
        engine.eval("(define conn (tcp-accept srv))").unwrap();
        assert!(
            val("(> conn 0)").contains("#t"),
            "accept failed: {}",
            val("(socket-last-error)")
        );
        assert!(val("(socket-recv-line conn)").contains("ping"));
        assert!(val(r#"(socket-send conn "pong\n")"#).contains("#t"));
        assert!(client.join().unwrap().contains("pong"));
        assert!(val("(socket-close conn)").contains("#t"));
        assert!(val("(tcp-listener-close srv)").contains("#t"));
    }

    #[test]
    fn udp_datagram_roundtrips_over_loopback() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // A std UdpSocket echoes one datagram back; the primitives send hex and
        // receive "addr\nhex".
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let sport = server.local_addr().unwrap().port();
        let echo = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            server.send_to(&buf[..n], from).unwrap();
        });
        engine.eval("(define u (udp-open))").unwrap();
        assert!(val("(> u 0)").contains("#t"));
        // 68656c6c6f = "hello"
        assert!(val(&format!(r#"(udp-send u "127.0.0.1" {sport} "68656c6c6f")"#)).contains("#t"));
        let got = val("(udp-recv u 64)");
        assert!(got.contains("68656c6c6f"), "udp echo wrong: {got}");
        echo.join().unwrap();
        assert!(val("(udp-close u)").contains("#t"));
    }

    #[test]
    fn async_socket_and_ws_primitives_queue_cleanly() {
        // The async socket / WebSocket / http-request-full workers run editor-
        // side (GPUI cx.spawn), so the harness can't drive them end-to-end —
        // same as http-request / make-process. This checks the registry
        // boundary: the primitives exist, type-check their args, mint a
        // positive id, and the send/close verbs return #t (they just enqueue an
        // EditorCommand).
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        assert!(
            val(r#"(socket-connect-async "example.com" 993 #t 'on-data 'on-close)"#)
                .parse::<i64>()
                .unwrap()
                > 0
        );
        assert!(val(r#"(socket-async-send 1 "6869")"#).contains("#t"));
        assert!(val("(socket-async-close 1)").contains("#t"));
        assert!(
            val(r#"(ws-connect "wss://example.com/socket" 'on-msg 'on-close)"#)
                .parse::<i64>()
                .unwrap()
                > 0
        );
        assert!(val(r#"(ws-send 1 "hello")"#).contains("#t"));
        assert!(val("(ws-close 1)").contains("#t"));
        // http-request / http-request-full both queue and return #t.
        assert!(val(r#"(http-request-full "GET" "https://x" '() "" 'k)"#).contains("#t"));
    }

    #[test]
    fn delete_file_removes_and_tolerates_absent() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        let dir = std::env::temp_dir().join(format!("macros-del-{}", std::process::id()));
        let f = dir.join("x.txt");
        let p = f.display();
        assert!(val(&format!(r#"(write-file "{p}" "hi")"#)).contains("#t"));
        assert!(val(&format!(r#"(delete-file "{p}")"#)).contains("#t"));
        assert!(val(&format!(r#"(file-exists? "{p}")"#)).contains("#f"));
        // Already-absent is #t (idempotent retract); a directory is #f.
        assert!(val(&format!(r#"(delete-file "{p}")"#)).contains("#t"));
        assert!(val(&format!(r#"(delete-file "{}")"#, dir.display())).contains("#f"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_primitives_fts5_roundtrip() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // In-memory db; create a plain table + an FTS5 virtual table, insert
        // bound rows, and query both ways.
        assert!(val(r#"(define db (sqlite-open ":memory:")) (> db 0)"#).contains("#t"));
        assert!(val(
            r#"(sqlite-exec db "CREATE TABLE t(id TEXT, n INT); CREATE VIRTUAL TABLE ft USING fts5(id, body);" (list))"#
        )
        .contains("#t"));
        assert!(
            val(r#"(sqlite-exec db "INSERT INTO t VALUES (?1, ?2)" (list "a" "7"))"#)
                .contains("#t")
        );
        // Bound params are injection-safe: this text is stored literally.
        assert!(val(r#"(sqlite-exec db "INSERT INTO ft VALUES (?1, ?2)" (list "m1" "invoice payment overdue"))"#).contains("#t"));
        assert!(
            val(
                r#"(sqlite-exec db "INSERT INTO ft VALUES (?1, ?2)" (list "m2" "lunch tomorrow"))"#
            )
            .contains("#t")
        );
        // Column values come back US-joined (0x1f); integer coerced to text.
        let row = val(r#"(car (sqlite-query db "SELECT id, n FROM t" (list)))"#);
        assert!(row.contains("a") && row.contains('7'), "row wrong: {row:?}");
        // FTS5 MATCH: "invoice" hits m1 only; ranked query returns its id.
        let hits = val(
            r#"(sqlite-query db "SELECT id FROM ft WHERE ft MATCH ?1 ORDER BY rank" (list "invoice"))"#,
        );
        assert!(
            hits.contains("m1") && !hits.contains("m2"),
            "fts wrong: {hits:?}"
        );
        // Phrase / prefix query.
        assert!(
            val(
                r#"(length (sqlite-query db "SELECT id FROM ft WHERE ft MATCH ?1" (list "pay*")))"#
            )
            .contains('1')
        );
        assert!(val("(sqlite-close db)").contains("#t"));
        // Ops on a closed/bogus id fail soft.
        assert!(val("(sqlite-exec 9999 \"SELECT 1\" (list))").contains("#f"));
        assert_eq!(val("(length (sqlite-query 9999 \"SELECT 1\" (list)))"), "0");
    }

    #[test]
    fn websocket_handshake_over_sock_roundtrips() {
        // Exercises the exact protocol path `ws_run` uses — tungstenite's client
        // handshake driven over our own `Sock` stream — against a loopback
        // tungstenite echo server. Proves the "no second TLS stack, we own the
        // stream" design frames correctly; the editor-side drain is separate.
        use tungstenite::Message;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let msg = ws.read().unwrap();
            if let Message::Text(t) = msg {
                ws.send(Message::Text(format!("echo:{t}").into())).unwrap();
            }
        });
        let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let sock = crate::script::Sock::Plain(tcp);
        let (mut ws, _) = tungstenite::client(format!("ws://127.0.0.1:{port}/"), sock).unwrap();
        ws.send(Message::Text("hi".into())).unwrap();
        let reply = ws.read().unwrap();
        assert_eq!(reply, Message::Text("echo:hi".into()));
        server.join().unwrap();
    }

    #[test]
    fn socket_family_bogus_ids_fail_soft() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        assert!(val(r#"(tcp-connect "127.0.0.1" 1 500 500)"#).contains('0'));
        assert!(val("(socket-send 9999 \"x\")").contains("#f"));
        assert_eq!(val("(string-length (socket-recv-line 9999))"), "0");
        assert!(val("(tls-upgrade 9999 \"x\")").contains("#f"));
        assert!(val("(socket-set-timeout 9999 100)").contains("#f"));
        assert!(val("(> (string-length (socket-last-error)) 0)").contains("#t"));
        assert!(val("(tcp-accept 9999)").contains('0'));
    }

    // Network tests (run manually: `cargo test <name> -- --ignored`). Ignored
    // by default so the suite stays offline-safe.
    #[test]
    #[ignore]
    fn dns_resolves_real_records() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // gmail.com always has MX records; each line is "<pref> <host>".
        let mx = val(r#"(dns-resolve "gmail.com" "MX")"#);
        assert!(mx.contains("google.com"), "no gmail MX: {mx}");
        // A record is dotted-quad.
        let a = val(r#"(dns-resolve "one.one.one.one" "A")"#);
        assert!(a.contains("1.1.1.1") || a.contains("1.0.0.1"), "no A: {a}");
    }

    #[test]
    #[ignore]
    fn tls_handshake_reads_imap_greeting() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        let _ = engine.eval(r#"(define tls-test-conn (tls-connect "imap.gmail.com" 993 0 0))"#);
        assert!(
            val("(> tls-test-conn 0)").contains("#t"),
            "connect failed: {}",
            val("(tls-last-error)")
        );
        let greeting = val("(tls-recv-line tls-test-conn)");
        assert!(greeting.contains("* OK"), "unexpected greeting: {greeting}");
        assert!(val("(tls-close tls-test-conn)").contains("#t"));
    }

    #[test]
    fn base64url_to_hex_accepts_both_alphabets() {
        let (engine, _rx) = ScriptEngine::new();
        let eval = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // "abc" is 616263; round-trips through the encoder.
        assert!(eval(r#"(base64url-hex (base64url-encode "abc"))"#).contains("616263"));
        // Standard alphabet + padding + a line wrap (MIME style): 0xfb 0xff
        // encodes as "+/8=" standard, "-_8" URL-safe — both must decode.
        assert!(eval("(base64url-hex \"+/\n8=\")").contains("fbff"));
        assert!(eval(r#"(base64url-hex "-_8")"#).contains("fbff"));
        // Garbage → "".
        assert!(eval(r#"(string-length (base64url-hex "!!!"))"#).contains('0'));
    }

    #[test]
    fn decode_charset_maps_legacy_bytes_to_utf8() {
        let (engine, _rx) = ScriptEngine::new();
        let eval = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // 0xe9 is é in both iso-8859-1 and windows-1252.
        assert!(eval(r#"(decode-charset "e9" "iso-8859-1")"#).contains('é'));
        // 0x93 is “ in windows-1252 (undefined in latin-1 proper).
        assert!(eval(r#"(decode-charset "93" "windows-1252")"#).contains('“'));
        // UTF-8 passthrough, and an unknown label falls back to UTF-8.
        assert!(eval(r#"(decode-charset "c3a9" "utf-8")"#).contains('é'));
        assert!(eval(r#"(decode-charset "c3a9" "no-such-charset")"#).contains('é'));
    }

    #[test]
    fn read_file_strict_refuses_what_read_file_blanks() {
        // Read-modify-write callers (org capture/archive/refile, magit's
        // .gitignore) used `read-file`, which yields "" for a file it can't
        // decode — and then wrote "" + the addition over it.
        let (engine, _rx) = ScriptEngine::new();
        let dir = std::env::temp_dir().join(format!("macros-strict-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("latin1.org");
        std::fs::write(&bad, b"* caf\xe9\n").unwrap();
        let eval = |src: String| engine.eval(&src);
        let lenient = eval(format!(
            r#"(string-length (read-file "{}"))"#,
            bad.display()
        ));
        assert_eq!(lenient.unwrap().value.as_deref(), Some("0"));
        let strict = eval(format!(r#"(read-file-strict "{}")"#, bad.display()));
        assert!(strict.is_err(), "invalid UTF-8 must raise");
        // A missing file is still "" (capture into a new file), and good text reads.
        let missing = eval(format!(
            r#"(string-length (read-file-strict "{}"))"#,
            dir.join("new.org").display()
        ));
        assert_eq!(missing.unwrap().value.as_deref(), Some("0"));
        let good = dir.join("ok.org");
        std::fs::write(&good, "* ok\n").unwrap();
        let text = eval(format!(r#"(read-file-strict "{}")"#, good.display()));
        assert!(text.unwrap().value.unwrap().contains("* ok"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_hex_round_trips_binary() {
        let (engine, _rx) = ScriptEngine::new();
        let dir = std::env::temp_dir().join(format!("macros-hex-test-{}", std::process::id()));
        let path = dir.join("blob.bin");
        // Bytes that are NOT valid UTF-8 (0xff 0xfe) — read-file would mangle.
        let src = format!(
            r#"(begin (write-file-hex "{p}" "fffe0001")
                      (read-file-hex "{p}"))"#,
            p = path.display()
        );
        let got = engine.eval(&src).unwrap().value.unwrap();
        assert!(got.contains("fffe0001"), "round-trip failed: {got}");
        // Bad hex refuses to write; missing file reads as "".
        let bad = engine
            .eval(r#"(write-file-hex "/tmp/x" "zz")"#)
            .unwrap()
            .value
            .unwrap();
        assert!(
            bad.contains("#f") || bad.contains("false"),
            "bad hex wrote: {bad}"
        );
        let missing = engine
            .eval(r#"(string-length (read-file-hex "/no/such/file"))"#)
            .unwrap()
            .value
            .unwrap();
        assert!(missing.contains('0'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Exercises the exact HTTP framing the `http-serve` worker thread uses
    // (request-line target parse + Content-Length response), proving a real
    // `ureq` client round-trips against it. The `http_serve` method itself needs
    // a live GPUI Context, so this validates the transport mechanics standalone.
    #[test]
    fn http_serve_transport_round_trips_with_ureq() {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = "you may close this tab";

        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // Same target extraction as the worker: 2nd token of the request line.
            let target = {
                let mut reader = BufReader::new(&conn);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                line.split_whitespace().nth(1).unwrap_or("").to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            conn.write_all(response.as_bytes()).unwrap();
            conn.flush().unwrap();
            target
        });

        let got = ureq::get(&format!(
            "http://127.0.0.1:{port}/callback?code=abc&state=xyz"
        ))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
        assert_eq!(got, body, "client did not read the fixed response body");

        let target = server.join().unwrap();
        assert_eq!(
            target, "/callback?code=abc&state=xyz",
            "request target parsed wrong (this is what on-request receives)"
        );
    }

    #[test]
    fn single_run_evaluates_forms_top_to_bottom() {
        let (engine, _rx) = ScriptEngine::new();
        // Both forms in ONE eval, exactly as a whole-region eval would submit
        // them: the later form must see the define from the earlier form.
        let result = engine.eval("(define (foobar a b) (+ a b))\n(foobar 10 10)");
        let value = result.ok().and_then(|r| r.value);
        assert_eq!(value.as_deref(), Some("20"));
    }

    #[test]
    fn last_sexp_only_misses_earlier_define() {
        let (engine, _rx) = ScriptEngine::new();
        // What `eval-last-sexp` (C-c C-e) actually submits: ONLY the last form.
        // foobar is never defined here, so this must error.
        let result = engine.eval("(foobar 10 10)");
        assert!(result.is_err());
    }

    #[test]
    fn bundled_theme_scripts_load_cleanly() {
        // Themes load on demand via `load-theme` (not with the startup
        // scripts), so a bad primitive call in one — e.g. a mistyped face
        // attribute setter — would only surface at runtime without this.
        let (engine, _rx) = ScriptEngine::new();
        for name in crate::runtime::theme_names() {
            let src = crate::runtime::theme_script(&name).expect("theme script");
            if let Err(e) = engine.eval(src) {
                panic!("theme {name} failed to load: {e}");
            }
        }
    }

    #[test]
    fn spell_pipe_parse_converts_ispell_char_offsets_to_bytes() {
        let (engine, _rx) = ScriptEngine::new();
        // ispell offsets are CHARACTER positions counting the `^` escape
        // prefix. Against line text "café apero wrld" at buffer byte 100:
        // "wrld" has 11 chars before it (char-off 12), but "café apero " is
        // 12 BYTES (é is 2) — the overlay range must be bytes [112, 116).
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        let _ = engine.eval(
            r#"(define spell-test-rec
                 (spell--pipe-parse "& wrld 46 12: weld, world"
                                    (list 100 "café apero wrld" #t)))"#,
        );
        assert_eq!(val("(nth spell-test-rec 0)"), "112");
        assert_eq!(val("(nth spell-test-rec 1)"), "116");
        assert!(val("(nth spell-test-rec 2)").contains("wrld"));
        assert_eq!(val("(length (nth spell-test-rec 3))"), "2");
        assert!(val("(car (nth spell-test-rec 3))").contains("weld"));

        // `#` records (no suggestions): "# <word> <offset>".
        let _ = engine.eval(
            r##"(define spell-test-rec2
                 (spell--pipe-parse "# wrld 12" (list 100 "café apero wrld" #t)))"##,
        );
        assert_eq!(val("(nth spell-test-rec2 0)"), "112");
        assert_eq!(val("(length (nth spell-test-rec2 3))"), "0");
    }

    #[test]
    fn spell_pipe_output_reassembles_chunks_and_fires_on_final_terminator() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        // A two-line region ("helllo world\nok wrld") queued as sent lines
        // (each entry carries the check-time edit tick + buffer, here matching
        // the live snapshot so the reply counts as fresh); the checker's reply
        // arrives split at awkward chunk boundaries.
        let _ = engine
            .eval(
                r#"(set! *spell-pipe-fifo*
                     (list (list 0 "helllo world" #f (buffer-tick) (current-buffer))
                           (list 13 "ok wrld" #t (buffer-tick) (current-buffer))))"#,
            )
            .unwrap();
        let _ = engine
            .eval(r#"(spell--pipe-output "& helllo 2 1: hello, hell\n\n& wr")"#)
            .unwrap();
        let _ = engine
            .eval(r#"(spell--pipe-output "ld 1 4: world\n\n")"#)
            .unwrap();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        // Both lines answered: errors are (0 6 helllo) and (13+3 13+7 wrld).
        assert_eq!(val("(length *spell-errors*)"), "2");
        assert_eq!(val("(car (car *spell-errors*))"), "0");
        assert_eq!(val("(nth (car *spell-errors*) 1)"), "6");
        assert_eq!(val("(car (car (cdr *spell-errors*)))"), "16");
        assert_eq!(val("(nth (car (cdr *spell-errors*)) 1)"), "20");
        // The suggestion cache picked up the `&` records' candidates.
        assert!(val(r#"(car (hash-try-get *spell-suggest-cache* "helllo"))"#).contains("hello"));
        // FIFO drained; nothing left buffered.
        assert_eq!(val("(length *spell-pipe-fifo*)"), "0");
        assert_eq!(val("(string-length *spell-pipe-buf*)"), "0");
    }

    #[test]
    fn spell_mode_toggle_round_trips_without_hanging() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        engine.set_snapshot(EditorSnapshot {
            name: "notes.org".into(),
            text: Rope::from_str("helllo wrld\nsome more text\n"),
            visible_lines: (0, 1),
            ..Default::default()
        });
        // Pin the backend so the test never probes the environment (CI has
        // neither the macOS checker nor aspell, and toggling on would
        // otherwise no-op there). The commands under test are generated by
        // pure Scheme — no OS spell call is made in this test.
        let _ = engine.eval("(set! *spell-backend* 'native)").unwrap();
        // Enabling must emit one SpellCheck over the visible region.
        let cmds = engine.eval("(spell-mode)").unwrap().commands;
        let checked = cmds.iter().find_map(|c| match c {
            EditorCommand::SpellCheck { text, callback } => Some((text.clone(), callback.clone())),
            _ => None,
        });
        let (text, callback) = checked.expect("spell-mode should queue a check");
        assert_eq!(text, "helllo wrld\nsome more text\n");
        // Feed the reply back exactly as the editor handler would; the
        // callback must paint squiggles at the absolute byte ranges.
        let cmds = engine.call_action(&callback, Some("0\t6\thelllo\n7\t11\twrld"));
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::OverlayFace { start: 7, end: 11, face } if face == "spell-error"
            )),
            "expected squiggle overlays, got {cmds:?}"
        );
        // Toggling off clears the squiggles.
        let cmds = engine.eval("(spell-mode)").unwrap().commands;
        assert!(cmds.iter().any(
            |c| matches!(c, EditorCommand::ClearOverlayFacesMatching(f) if f == "spell-error")
        ));
    }

    /// Spell replies guard their squiggle batch with the buffer name + edit
    /// tick captured when the text was SENT for checking (threaded through the
    /// reply FIFO), mirroring `org_fontify_batch_is_guarded`: a reply landing
    /// on an unchanged buffer paints behind a `GuardBufferUnchanged`; one that
    /// arrives after an edit paints nothing and reschedules the check instead
    /// of caching squiggles at shifted offsets.
    #[test]
    fn spell_result_batches_are_guarded() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let snapshot = |tick: u64| EditorSnapshot {
            name: "notes.org".into(),
            tick,
            text: Rope::from_str("helllo wrld\nsome more text\n"),
            visible_lines: (0, 1),
            ..Default::default()
        };
        engine.set_snapshot(snapshot(5));
        let _ = engine.eval("(set! *spell-backend* 'native)").unwrap();
        // Enabling runs the first check: the FIFO records (start tick buffer).
        engine.eval("(spell-mode)").unwrap();
        // Reply with the buffer untouched (tick still 5): the batch opens with
        // the check-time guard, then paints — so an edit racing the batch's
        // application still drops it in the editor.
        let cmds = engine
            .eval(r#"(spell--native-apply "0\t6\thelllo\n")"#)
            .unwrap()
            .commands;
        assert!(
            matches!(
                cmds.first(),
                Some(EditorCommand::GuardBufferUnchanged { name, tick: 5, on_stale })
                    if name == "notes.org" && on_stale == "spell--schedule"
            ),
            "a fresh reply must open with the check-time guard: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::OverlayFace { start: 0, end: 6, face } if face == "spell-error"
            )),
            "a fresh reply should paint the squiggle: {cmds:?}"
        );
        // Second check pass (FIFO entry captured at tick 5), then an edit
        // before the reply arrives (tick 6): stale — nothing painted or
        // cleared, just a rescheduled check.
        engine.eval("(spell--check-now)").unwrap();
        engine.set_snapshot(snapshot(6));
        let cmds = engine
            .eval(r#"(spell--native-apply "0\t6\thelllo\n")"#)
            .unwrap()
            .commands;
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                EditorCommand::OverlayFace { .. }
                    | EditorCommand::ClearOverlayFacesMatching(_)
                    | EditorCommand::GuardBufferUnchanged { .. }
            )),
            "a stale reply must not touch overlays: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::RunWithTimer { .. })),
            "a stale reply should reschedule the check: {cmds:?}"
        );
    }

    #[test]
    fn spell_corrections_open_a_helm_session_with_actions() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let cmds = engine
            .eval(r#"(spell--offer-corrections "helllo" 5 11 '("hello" "he'll"))"#)
            .unwrap()
            .commands;
        let sources = match cmds.as_slice() {
            [EditorCommand::Helm(sources)] => sources,
            other => panic!("expected a Helm session, got {other:?}"),
        };
        assert_eq!(sources[0].name, "Corrections: helllo");
        assert!(matches!(
            &sources[0].candidates,
            HelmCandidates::Static(c) if c.len() == 2
        ));
        // Default action replaces the misspelled range with the selection.
        let (label, action) = &sources[0].actions[0];
        assert_eq!(label, "Replace");
        let cmds = engine.call_action(action, Some("hello"));
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::ReplaceRegion { start: 5, end: 11, text } if text == "hello"
            )),
            "expected the replacement edit, got {cmds:?}"
        );
        // M-a extras: learn / ignore the ORIGINAL word.
        assert_eq!(sources[0].actions.len(), 3);
        assert!(sources[0].actions[1].0.contains("dictionary"));
        assert!(sources[0].actions[2].0.contains("Ignore"));
    }

    #[test]
    fn spell_error_navigation_walks_cached_ranges() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let _ = engine.eval(
            r#"(begin
                 (spell--set-errors '((5 10 "aa") (20 25 "bb")))
                 (set! *spell-errors-buffer* (current-buffer)))"#,
        );
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        assert_eq!(val("(car (spell--error-after 10))"), "20");
        assert_eq!(val("(car (spell--error-before 15))"), "5");
        assert_eq!(val("(car (spell--error-at 7))"), "5");
        assert_eq!(val("(spell--error-after 20)"), "#false");
        assert_eq!(val("(spell--error-at 12)"), "#false");
    }

    #[test]
    fn helm_parses_sources_from_scheme() {
        let (engine, _rx) = ScriptEngine::new();
        let result = engine
            .eval(r#"(helm (list (list "Files" (list "a.txt" "b.txt") "open-file")))"#)
            .unwrap();
        match result.commands.as_slice() {
            [EditorCommand::Helm(sources)] => {
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].name, "Files");
                assert!(matches!(
                    &sources[0].candidates,
                    HelmCandidates::Static(c)
                        if c == &[("a.txt".to_string(), None), ("b.txt".to_string(), None)]
                ));
                assert_eq!(
                    sources[0].actions,
                    [("default".to_string(), "open-file".to_string())]
                );
            }
            other => panic!("expected one Helm command, got {other:?}"),
        }
    }

    #[test]
    fn helm_parses_dynamic_source_and_multiple_actions() {
        let (engine, _rx) = ScriptEngine::new();
        // Candidates as a function name (dynamic); actions as a (label fn) list.
        let result = engine
            .eval(
                r#"(helm (list (list "Files" "list-them"
                       (list (list "open" "open-file") (list "delete" "rm-file")))))"#,
            )
            .unwrap();
        match result.commands.as_slice() {
            [EditorCommand::Helm(sources)] => {
                assert!(matches!(
                    &sources[0].candidates,
                    HelmCandidates::Dynamic(f) if f == "list-them"
                ));
                assert_eq!(
                    sources[0].actions,
                    [
                        ("open".to_string(), "open-file".to_string()),
                        ("delete".to_string(), "rm-file".to_string()),
                    ]
                );
            }
            other => panic!("expected one Helm command, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_rank_orders_and_filters() {
        let cands = vec![
            "foo/bar.rs".to_string(),
            "fxxxbxxxr.rs".to_string(),
            "unrelated.txt".to_string(),
        ];
        // Subsequence matches only; the non-match is dropped, tighter ranks first.
        let ranked = fuzzy_rank("fbr", &cands);
        assert_eq!(ranked.first().map(String::as_str), Some("foo/bar.rs"));
        assert!(!ranked.iter().any(|c| c == "unrelated.txt"));
        // Empty pattern is identity (order preserved).
        assert_eq!(fuzzy_rank("", &cands), cands);
    }

    #[test]
    fn helm_parses_raw_dynamic_source() {
        let (engine, _rx) = ScriptEngine::new();
        // `(raw "fn")` candidates form → a self-filtered dynamic source.
        let result = engine
            .eval(r#"(helm (list (list "Files" (list 'raw "my-filter") "open-file")))"#)
            .unwrap();
        match result.commands.as_slice() {
            [EditorCommand::Helm(sources)] => assert!(matches!(
                &sources[0].candidates,
                HelmCandidates::DynamicRaw(f) if f == "my-filter"
            )),
            other => panic!("expected one Helm command, got {other:?}"),
        }
    }

    #[test]
    fn helm_raw_marker_does_not_capture_string_candidates() {
        let (engine, _rx) = ScriptEngine::new();
        // A static list whose first entry is the *string* "raw" stays static —
        // only a leading `raw` symbol marks a self-filtered source.
        let result = engine
            .eval(r#"(helm (list (list "S" (list "raw" "cooked") "open-file")))"#)
            .unwrap();
        match result.commands.as_slice() {
            [EditorCommand::Helm(sources)] => assert!(matches!(
                &sources[0].candidates,
                HelmCandidates::Static(c)
                    if c == &[("raw".to_string(), None), ("cooked".to_string(), None)]
            )),
            other => panic!("expected one Helm command, got {other:?}"),
        }
    }

    #[test]
    fn add_and_remove_hook_parse() {
        let (engine, _rx) = ScriptEngine::new();
        let added = engine.eval(r#"(add-hook "steel" "enable-lsp")"#).unwrap();
        assert!(matches!(
            added.commands.as_slice(),
            [EditorCommand::AddHook(m, f)] if m == "steel" && f == "enable-lsp"
        ));
        let removed = engine
            .eval(r#"(remove-hook "steel" "enable-lsp")"#)
            .unwrap();
        assert!(matches!(
            removed.commands.as_slice(),
            [EditorCommand::RemoveHook(m, f)] if m == "steel" && f == "enable-lsp"
        ));
    }

    /// With core.scm loaded, `add-hook` accepts a 'symbol and a procedure (not
    /// just a "name" string), mirroring `bind-key`. A symbol defers to its name;
    /// a lambda is stashed behind a generated, callable global stub.
    #[test]
    fn add_hook_accepts_symbol_and_lambda() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/hooks.scm"))
            .expect("hooks.scm");

        // A 'symbol resolves to its bare name — same registration a string makes.
        let sym = engine.eval(r#"(add-hook "rust" 'my-setup)"#).unwrap();
        assert!(
            matches!(
                sym.commands.as_slice(),
                [EditorCommand::AddHook(m, f)] if m == "rust" && f == "my-setup"
            ),
            "symbol hook should register by name: {:?}",
            sym.commands
        );

        // A lambda registers under a generated stub name (it can't be a bare
        // identifier the editor calls otherwise).
        let lam = engine
            .eval(r#"(begin (define *probe* 0) (add-hook "rust" (lambda () (set! *probe* 42))))"#)
            .unwrap();
        let stub = match lam.commands.as_slice() {
            [EditorCommand::AddHook(m, f)] if m == "rust" => {
                assert!(f.starts_with("hook-fn-"), "lambda stub name was {f:?}");
                f.clone()
            }
            other => panic!("lambda hook should enqueue one AddHook: {other:?}"),
        };
        // The stub is a real global: calling it (how the editor invokes a hook by
        // name) runs the original closure.
        engine.eval(&format!("({stub})")).unwrap();
        assert_eq!(
            engine.eval("*probe*").unwrap().value.as_deref(),
            Some("42"),
            "calling the generated stub should run the lambda"
        );
    }

    /// `run-hooks` fires each hook in order — one RunHook per argument, and
    /// 'symbol arguments resolve to their names like `add-hook` accepts.
    #[test]
    fn run_hooks_fires_each_in_order() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/hooks.scm"))
            .expect("hooks.scm");
        let out = engine
            .eval(r#"(run-hooks "find-file-hook" 'after-save-hook "bookmark-save-hook")"#)
            .unwrap();
        let hooks: Vec<String> = out
            .commands
            .iter()
            .filter_map(|c| match c {
                EditorCommand::RunHook(h) => Some(h.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            hooks,
            ["find-file-hook", "after-save-hook", "bookmark-save-hook"],
            "one RunHook per hook, in order: {:?}",
            out.commands
        );
        // Zero arguments: no commands at all.
        assert!(engine.eval("(run-hooks)").unwrap().commands.is_empty());
    }

    /// `register-command` / `define-command` accept the canonical 'symbol form
    /// (strings still work), and `define-key` binds a 'symbol command by name.
    #[test]
    fn command_registration_accepts_symbols() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");

        // register-command with a 'symbol stores the bare name string.
        engine.eval("(register-command 'foo-cmd)").unwrap();
        assert_eq!(
            engine
                .eval(r#"(if (member "foo-cmd" *interactive-commands*) 1 0)"#)
                .unwrap()
                .value
                .as_deref(),
            Some("1"),
            "register-command should accept a 'symbol"
        );

        // define-command defines the function AND registers its name.
        let ran = engine
            .eval("(begin (define-command (bar-cmd) 99) (bar-cmd))")
            .unwrap();
        assert_eq!(ran.value.as_deref(), Some("99"), "define-command body runs");
        assert_eq!(
            engine
                .eval(r#"(if (member "bar-cmd" *interactive-commands*) 1 0)"#)
                .unwrap()
                .value
                .as_deref(),
            Some("1"),
            "define-command should register the name"
        );

        // define-key resolves a 'symbol command to its bare name.
        let bound = engine
            .eval(r#"(define-key "fundamental" "ctrl-x z" 'my-cmd)"#)
            .unwrap();
        assert!(
            matches!(
                bound.commands.as_slice(),
                [EditorCommand::DefineKey(m, c, cmd)]
                    if m == "fundamental" && c == "ctrl-x z" && cmd == "my-cmd"
            ),
            "define-key should bind a 'symbol by name: {:?}",
            bound.commands
        );
    }

    /// Language servers are opt-in: loading the full bundled runtime configures
    /// only the built-in (in-process) Scheme servers — no external server is
    /// wired up until the user declares one in their init.scm.
    #[test]
    fn language_servers_are_opt_in_by_default() {
        let (engine, _rx) = ScriptEngine::new();
        let mut configured: Vec<String> = Vec::new();
        for src in crate::runtime::startup_scripts() {
            let out = engine.eval(src).expect("a startup script failed to load");
            for c in out.commands {
                if let EditorCommand::SetLsp(lang, ..) = c {
                    configured.push(lang);
                }
            }
        }
        // The built-in Scheme servers stay on (no binary, power config editing).
        assert!(
            configured.iter().any(|l| l == "macros-scheme"),
            "built-in macros-scheme server should be configured: {configured:?}"
        );
        // No external language server is configured out of the box.
        for ext in [
            "rust",
            "go",
            "c",
            "cpp",
            "typescript",
            "javascript",
            "python",
        ] {
            assert!(
                !configured.iter().any(|l| l == ext),
                "{ext} should NOT be configured by default (servers are opt-in): {configured:?}"
            );
        }
    }

    #[test]
    fn evil_bind_command_by_identity() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");

        // Bind by IDENTITY: a bare command procedure (a builtin here), not a
        // "name" string — so editor tooling sees a real reference.
        engine
            .eval(r#"(evil-define-key "normal" "rust" "space g l" lsp-find-references)"#)
            .expect("bind by identity");
        // The keypress evals the stored dispatcher, which calls the stored
        // procedure, which enqueues the real command.
        let cmds = engine
            .eval(r#"(evil-run-key "evil-normal:rust space g l")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::LspReferences)),
            "identity binding should dispatch LspReferences: {cmds:?}"
        );

        // Canonical config form: a 'name symbol binds directly (no table entry),
        // the same as the string path — resolution is deferred to keypress.
        let cmds = engine
            .eval(r#"(evil-define-key "normal" "zig" "space g l" 'lsp-find-references)"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::DefineKey(m, s, cmd)
                    if m == "evil-normal:zig" && s == "space g l" && cmd == "lsp-find-references")),
            "symbol binding should define-key directly: {cmds:?}"
        );

        // Back-compat: a "name" string still binds directly (no table entry).
        let cmds = engine
            .eval(r#"(evil-define-key "normal" "go" "space g l" "lsp-find-references")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::DefineKey(m, s, cmd)
                    if m == "evil-normal:go" && s == "space g l" && cmd == "lsp-find-references")),
            "name-string binding should define-key directly: {cmds:?}"
        );
    }

    #[test]
    fn evil_ctrl_chords_bound_in_evil_keymap() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        let cmds = engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm")
            .commands;
        let bound = |mode: &str, chord: &str, to: &str| {
            cmds.iter().any(|c| {
                matches!(c, EditorCommand::DefineKey(m, s, cmd)
                if m == mode && s == chord && cmd == to)
            })
        };
        // C-v must enter block-wise visual — otherwise it falls through to the
        // Emacs `ctrl-v` = scroll-up and the cursor jumps a page toward the bottom.
        assert!(
            bound("evil-normal", "ctrl-v", "evil-visual-block-state"),
            "C-v should enter block-wise visual in evil-normal"
        );
        // The half-page scroll chords stay bound in the evil keymap (the Rust
        // dispatch now consults it for Ctrl chords, so these actually fire).
        assert!(bound("evil-normal", "ctrl-d", "evil-scroll-down"));
        assert!(bound("evil-normal", "ctrl-u", "evil-scroll-up"));
    }

    #[test]
    fn evil_edit_operators_and_motions_bound() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        let cmds = engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm")
            .commands;
        let bound = |mode: &str, chord: &str, to: &str| {
            cmds.iter().any(|c| {
                matches!(c, EditorCommand::DefineKey(m, s, cmd)
                if m == mode && s == chord && cmd == to)
            })
        };
        // Single-key edits: `x` must delete INTO the register (not the plain
        // `delete-char`), and `r`/`s`/`S`/`~`/`J` must be bound.
        assert!(bound("evil-normal", "x", "evil-delete-char"));
        assert!(bound("evil-normal", "r", "evil-replace-char"));
        assert!(bound("evil-normal", "s", "evil-substitute-char"));
        assert!(bound("evil-normal", "shift-s", "evil-change-line"));
        assert!(bound("evil-normal", "~", "evil-toggle-case"));
        assert!(bound("evil-normal", "shift-j", "evil-join-line"));
        // Motions: end-of-word, first-non-blank, and the Vim-correct gg/G.
        assert!(bound("evil-normal", "e", "evil-forward-word-end"));
        assert!(bound("evil-normal", "^", "evil-first-non-blank"));
        assert!(bound("evil-normal", "g g", "evil-goto-first-line"));
        assert!(bound("evil-normal", "shift-g", "evil-goto-last-line"));
        // d / c / y are operators (the grammar resolves the motion), plus the
        // D / C shorthands.
        assert!(bound("evil-normal", "d", r#"evil-operator "delete""#));
        assert!(bound("evil-normal", "c", r#"evil-operator "change""#));
        assert!(bound("evil-normal", "y", r#"evil-operator "yank""#));
        assert!(bound("evil-normal", ">", r#"evil-operator "indent""#));
        assert!(bound("evil-operator", "w", r#"evil-motion "w""#));
        assert!(bound("evil-operator", "$", r#"evil-motion "$""#));
        assert!(bound("evil-operator", "g g", r#"evil-motion "gg""#));
        assert!(bound("evil-operator", "f", r#"evil-motion-find "f" %c"#));
        assert!(bound("evil-normal", "shift-d", "evil-delete-to-eol"));
        assert!(bound("evil-normal", "shift-c", "evil-change-to-eol"));
        // `.` repeats the last change; it takes a count (`3.`).
        assert!(bound("evil-normal", ".", "evil-repeat"));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetCountAware(n) if n == "evil-repeat"))
        );
        // Visual additions: change/substitute, swap-ends, and case folding.
        assert!(bound("evil-visual", "c", "evil-visual-change"));
        assert!(bound("evil-visual", "s", "evil-visual-change"));
        assert!(bound("evil-visual", "o", "exchange-point-and-mark"));
        assert!(bound("evil-visual", "u", "evil-visual-downcase"));
        assert!(bound("evil-visual", "shift-u", "evil-visual-upcase"));
        assert!(bound("evil-visual", "e", "evil-forward-word-end"));
        assert!(bound("evil-visual-line", "c", "evil-visual-line-change"));
        assert!(bound("evil-visual-line", "u", "evil-visual-downcase"));
        assert!(bound("evil-visual-line", "shift-u", "evil-visual-upcase"));
        // gq/gw — the format (fill to *fill-column*) operator family.
        assert!(bound("evil-normal", "g q", r#"evil-operator "fill""#));
        assert!(bound("evil-normal", "g w", r#"evil-operator "fill-keep""#));
        assert!(bound(
            "evil-normal",
            "g shift-u",
            r#"evil-operator "upcase""#
        ));
        assert!(bound("evil-visual", "g q", "evil-visual-fill"));
        assert!(bound("evil-visual-line", "g q", "evil-visual-fill"));
        // C-r must redo — unbound it falls through to Emacs isearch-backward.
        assert!(bound("evil-normal", "ctrl-r", "redo"));
        assert!(bound("evil-normal", "shift-y", "evil-yank-line")); // Y = yy
        // Char find, repeat, word search, paragraph motion, matchit.
        assert!(bound("evil-normal", "f", "evil-find-char-forward"));
        assert!(bound("evil-normal", "t", "evil-till-char-forward"));
        assert!(bound("evil-normal", ";", "evil-find-repeat"));
        assert!(bound("evil-normal", ",", "evil-find-repeat-reverse"));
        assert!(bound("evil-normal", "*", "evil-search-word-forward"));
        assert!(bound("evil-normal", "#", "evil-search-word-backward"));
        assert!(bound("evil-normal", "{", "evil-backward-paragraph"));
        assert!(bound("evil-normal", "}", "evil-forward-paragraph"));
        assert!(bound("evil-normal", "%", "evil-matchit"));
        // Scroll/recenter, LSP conventions, macros, case/join extras.
        assert!(bound("evil-normal", "z z", "recenter"));
        assert!(bound("evil-normal", "ctrl-f", "scroll-up"));
        assert!(bound("evil-normal", "ctrl-b", "scroll-down"));
        assert!(bound("evil-normal", "g d", "lsp-find-definition"));
        assert!(bound("evil-normal", "shift-k", "lsp-hover"));
        assert!(bound("evil-normal", "q", "evil-kbd-macro-toggle"));
        assert!(bound("evil-normal", "@", "evil-macro-play %c"));
        assert!(bound("evil-normal", "\"", "evil-use-register %c"));
        assert!(bound("evil-visual", "\"", "evil-use-register %c"));
        assert!(bound("evil-insert", "ctrl-r", "evil-insert-register %c"));
        assert!(bound("evil-normal", "g shift-j", "evil-join-line-no-space"));
        assert!(bound("evil-normal", "g u", r#"evil-operator "downcase""#));
        // Visual: paste-over, case toggle, extend-flavored find.
        assert!(bound("evil-visual", "p", "evil-visual-paste"));
        assert!(bound("evil-visual", "~", "evil-visual-toggle-case"));
        assert!(bound("evil-visual", "f", "evil-find-char-forward-visual"));
        assert!(bound("evil-visual-line", "p", "evil-visual-paste"));
        // Text objects are generated into the operator keymap: spot-check.
        assert!(bound("evil-operator", "i w", r#"evil-object "i" "word""#));
        assert!(bound("evil-operator", "a (", r#"evil-object "a" "paren""#));
        assert!(bound(
            "evil-operator",
            "i \"",
            r#"evil-object "i" "dquote""#
        ));
        assert!(bound(
            "evil-visual",
            "i w",
            r#"evil-object-cmd "select" "i" "word""#
        ));
    }

    /// The pure wrap engine behind M-q and gq: greedy re-flow at a width,
    /// keeping the first line's indentation and comment leader on every line.
    #[test]
    fn fill_string_wraps_indent_and_comment_leaders() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        let check = |src: &str| {
            let q = format!(r#"(if {src} "y" "n")"#);
            assert_eq!(
                engine.eval(&q).unwrap().value.as_deref(),
                Some("\"y\""),
                "{src}"
            );
        };
        // Words re-flow greedily at the width; a short paragraph is unchanged.
        check(r#"(equal? (fill-string "aaa bbb ccc ddd" 7) "aaa bbb\nccc ddd")"#);
        check(r#"(equal? (fill-string "hello world" 80) "hello world")"#);
        // The first line's indentation starts every wrapped line.
        check(r#"(equal? (fill-string "  aaa bbb ccc" 8) "  aaa\n  bbb\n  ccc")"#);
        // A comment leader continues onto wrapped lines (Vim formatoptions `q`)
        // and short comment lines join back up under it.
        check(r#"(equal? (fill-string ";; aaa bbb" 7) ";; aaa\n;; bbb")"#);
        check(r#"(equal? (fill-string ";; aaa\n;; bbb" 80) ";; aaa bbb")"#);
        // "#hashtag" is a word, not a leader (a leader needs whitespace after).
        check(r##"(equal? (fill-string "#tag word" 80) "#tag word")"##);
        // An over-long word gets its own line — never hard-broken.
        check(r#"(equal? (fill-string "aaaaaaaaaa bb" 4) "aaaaaaaaaa\nbb")"#);
        // fill-text wraps each paragraph and keeps the blank separator lines.
        check(r#"(equal? (fill-text "aaa bbb\n\nccc ddd" 3) "aaa\nbbb\n\nccc\nddd")"#);
    }

    /// gq end to end under the snapshot model: read the range, wrap it, issue
    /// one replace-region, then park the cursor at the start.
    #[test]
    fn evil_gq_formats_to_fill_column() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");

        // gqq — a long line re-wraps at *fill-column* (80).
        let long = "word ".repeat(20).trim_end().to_string(); // 99 chars
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str(&format!("{long}\n")),
            point: 0,
            ..Default::default()
        });
        let cmds = engine.eval("(evil-fill-line)").unwrap().commands;
        match cmds.as_slice() {
            [
                EditorCommand::ReplaceRegion {
                    start: 0,
                    end: 99,
                    text,
                },
                EditorCommand::GotoChar(0),
            ] => {
                assert!(text.contains('\n'), "a 99-char line should wrap: {text:?}");
                assert!(
                    text.lines().all(|l| l.len() <= 80),
                    "wrapped past column 80: {text:?}"
                );
                assert_eq!(
                    text.split_whitespace().count(),
                    20,
                    "words lost in the wrap: {text:?}"
                );
            }
            other => panic!("expected ReplaceRegion + GotoChar, got {other:?}"),
        }

        // gqip — the blank-line-delimited paragraph around point re-wraps as
        // one unit; the paragraph past the blank line is untouched.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("aaa\nbbb\n\nccc\n"),
            point: 5, // inside "bbb"
            ..Default::default()
        });
        let cmds = engine.eval("(evil-fill-paragraph)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 0, end: 7, text }
                if text == "aaa bbb")
            ),
            "gqip should re-wrap exactly the first paragraph: {cmds:?}"
        );
    }

    /// The Emacs word/paragraph editing commands added alongside M-q: word
    /// case folding, transpose-words, whitespace cleanup — all snapshot-read →
    /// one replace-region.
    #[test]
    fn emacs_word_and_paragraph_commands() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/editing.scm"))
            .expect("editing.scm");
        let set = |text: &str, point: usize| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                point,
                ..Default::default()
            });
        };

        // M-u — upcase from point to end of word, cursor moves past it.
        set("hello world\n", 0);
        let cmds = engine.eval("(upcase-word)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 0, end: 5, text }
                if text == "HELLO")
            ),
            "upcase-word should fold the word at point: {cmds:?}"
        );

        // M-t — swap the words around point ("foo| bar" → "bar foo").
        set("foo bar\n", 3);
        let cmds = engine.eval("(transpose-words)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 0, end: 7, text }
                if text == "bar foo")
            ),
            "transpose-words should swap around point: {cmds:?}"
        );

        // M-SPC — collapse the whitespace run around point to a single space.
        set("a   b\n", 2);
        let cmds = engine.eval("(just-one-space)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 1, end: 4, text }
                if text == " ")
            ),
            "just-one-space should collapse the run: {cmds:?}"
        );

        // M-} — forward to the blank line after the paragraph.
        set("aaa\nbbb\n\nccc\n", 0);
        let cmds = engine.eval("(forward-paragraph)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(8))),
            "forward-paragraph should land on the blank line: {cmds:?}"
        );

        // C-x C-o — a run of blank lines collapses to one.
        set("aaa\n\n\n\nbbb\n", 4);
        let cmds = engine.eval("(delete-blank-lines)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 4, end: 7, text }
                if text == "\n")
            ),
            "delete-blank-lines should leave one blank line: {cmds:?}"
        );
    }

    /// f/t char find and * word search: the scanner runs on the snapshot; the
    /// arming side goes through the `read-char` capture command.
    #[test]
    fn evil_find_char_and_search_word() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("hello world\n"),
            point: 0,
            ..Default::default()
        });

        // Arming `f` emits the read-char capture with the dispatch callback.
        let cmds = engine.eval("(evil-find-char-forward)").unwrap().commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::ReadChar { prompt, callback }
                if prompt == "f-" && callback == "evil-find-dispatch")),
            "f should arm a read-char capture: {cmds:?}"
        );

        // The dispatch itself: f→"o" lands ON the first o; t stops one short;
        // the visual flavor extends via select-to instead of moving.
        let cmds = engine.eval(r#"(evil-find-dispatch "o")"#).unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(4))),
            "f o should land on the o of hello: {cmds:?}"
        );
        let cmds = engine
            .eval(r#"(set! *evil-find-till* #t) (evil-find-dispatch "o")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(3))),
            "t o should stop before the o: {cmds:?}"
        );
        let cmds = engine
            .eval(r#"(set! *evil-find-till* #f) (evil-find-dispatch-visual "o")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::SelectTo(4))),
            "visual f o should extend the selection: {cmds:?}"
        );

        // Backward find with a multibyte target lands on the char's START
        // byte. (Regression: a byte-stepping scan false-matched at a
        // mid-codepoint offset — buffer-substring floors to the char start —
        // handing goto-char an unaligned position.)
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("x€éy\n"),
            point: 6, // on the y (€ is bytes 1..4, é is 4..6)
            ..Default::default()
        });
        let cmds = engine
            .eval(r#"(set! *evil-find-forward* #f) (evil-find-dispatch "€")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(1))),
            "F € must land on the char's first byte, never mid-codepoint: {cmds:?}"
        );
        engine.eval("(set! *evil-find-forward* #t)").unwrap();

        // ; repeats with the stored char, , goes the other way.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("ao bo co\n"),
            point: 4,
            ..Default::default()
        });
        engine.eval(r#"(set! *evil-find-char* "o")"#).unwrap();
        let cmds = engine.eval("(evil-find-repeat)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(7))),
            "; should repeat forward to the next o: {cmds:?}"
        );
        let cmds = engine.eval("(evil-find-repeat-reverse)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(1))),
            ", should search backward to the previous o: {cmds:?}"
        );

        // * — whole-word regex from the word at point, stored + jumped.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("foo bar foo\n"),
            point: 0,
            ..Default::default()
        });
        let cmds = engine.eval("(evil-search-word-forward)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::SetSearchQuery { query, forward: true }
                if query == "\\bfoo\\b")
            ),
            "* should store a whole-word query: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::EvilSearchNext)),
            "* should jump to the next match: {cmds:?}"
        );
    }

    /// Text objects: word/quote/pair/paragraph ranges and the operator
    /// plumbing (delete → kill ring, yank → region save, select → set-region).
    #[test]
    fn evil_text_objects() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");
        let set = |text: &str, point: usize| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                point,
                ..Default::default()
            });
        };

        // diw / daw — the word under point, without / with trailing space.
        set("foo bar baz\n", 5);
        let cmds = engine
            .eval(r#"(evil-object-cmd "delete" "i" "word")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::KillRegionBetween { start: 4, end: 7 })),
            "diw should kill exactly the word: {cmds:?}"
        );
        set("foo bar baz\n", 5);
        let cmds = engine
            .eval(r#"(evil-object-cmd "delete" "a" "word")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::KillRegionBetween { start: 4, end: 8 })),
            "daw should include the trailing space: {cmds:?}"
        );

        // ci" — inside the quotes; the change enters insert state.
        set("x \"abc\" y\n", 4);
        let cmds = engine
            .eval(r#"(evil-object-cmd "change" "i" "dquote")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::KillRegionBetween { start: 3, end: 6 })),
            "ci\" should kill between the quotes: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::EvilInsertState)),
            "change should enter insert state: {cmds:?}"
        );

        // da( — the innermost paren pair containing point, brackets included.
        set("f(a(b)c)\n", 4);
        let cmds = engine
            .eval(r#"(evil-object-cmd "delete" "a" "paren")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::KillRegionBetween { start: 3, end: 6 })),
            "da( should take the innermost pair: {cmds:?}"
        );

        // yiw — select + kill-ring-save (registers AND clipboard), cursor back.
        set("foo bar baz\n", 5);
        let cmds = engine
            .eval(r#"(evil-object-cmd "yank" "i" "word")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetRegion { start: 4, end: 7 })),
            "yiw should mark the word: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::KillRingSave)),
            "yiw should save via the kill ring: {cmds:?}"
        );

        // dip — the paragraph's lines, marked line-wise for the next paste.
        set("aaa\nbbb\n\nccc\n", 5);
        let cmds = engine
            .eval(r#"(evil-object-cmd "delete" "i" "para")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::KillRegionBetween { start: 0, end: 8 })),
            "dip should kill the paragraph's lines: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::EvilSetLinewise)),
            "dip is line-wise: {cmds:?}"
        );

        // % — bounce between a bracket and its match, nesting-aware.
        set("(foo)\n", 0);
        let cmds = engine.eval("(evil-matchit)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(4))),
            "% on ( should jump to ): {cmds:?}"
        );
        set("(a(b)c)\n", 6);
        let cmds = engine.eval("(evil-matchit)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(0))),
            "% on the outer ) should jump to the outer (: {cmds:?}"
        );
    }

    /// The Vim operator grammar: `(evil-operate op count force thunk)` turns a
    /// motion result into a range with Vim's exclusive / inclusive / linewise
    /// rules and applies the operator.
    #[test]
    fn evil_operator_grammar_ranges() {
        use EditorCommand as C;
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");
        let run = |text: &str, point: usize, src: &str| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                point,
                selections: vec![(point, point)],
                evil_state: "normal".into(),
                visible_lines: (0, 100),
                ..Default::default()
            });
            engine.eval(src).unwrap().commands
        };
        let op = |op: &str, count: usize, force: &str, motion: &str| {
            format!("(evil-operate \"{op}\" {count} \"{force}\" (lambda () {motion}))")
        };
        let kill = |c: &[EditorCommand], s: usize, e: usize| {
            c.iter().any(
                |c| matches!(c, C::KillRegionBetween { start, end } if *start == s && *end == e),
            )
        };
        // dw / 2d3w (count multiplies) / dw on a line's last word (no join).
        let c = run("foo bar", 0, &op("delete", 0, "", r#"(evil-motion "w")"#));
        assert!(kill(&c, 0, 4), "dw: {c:?}");
        let c = run(
            "a b c d e f g h",
            0,
            &op("delete", 6, "", r#"(evil-motion "w")"#),
        );
        assert!(kill(&c, 0, 12), "2d3w: {c:?}");
        let c = run(
            "foo bar\n  baz",
            4,
            &op("delete", 0, "", r#"(evil-motion "w")"#),
        );
        assert!(kill(&c, 4, 7), "dw at eol: {c:?}");
        // cw is ce from inside a word, and enters Insert.
        let c = run(
            "hello world",
            0,
            &op("change", 0, "", r#"(evil-motion "w")"#),
        );
        assert!(kill(&c, 0, 5), "cw: {c:?}");
        assert!(c.iter().any(|c| matches!(c, C::EvilInsertState)), "{c:?}");
        // e is inclusive; ge inclusive backward; $ to the line end.
        let c = run("foo bar", 0, &op("delete", 0, "", r#"(evil-motion "e")"#));
        assert!(kill(&c, 0, 3), "de: {c:?}");
        let c = run("foo bar", 4, &op("delete", 0, "", r#"(evil-motion "ge")"#));
        assert!(kill(&c, 2, 5), "dge: {c:?}");
        let c = run(
            "abc def\nx",
            2,
            &op("delete", 0, "", r#"(evil-motion "$")"#),
        );
        assert!(kill(&c, 2, 7), "d$: {c:?}");
        // j / dd / 3dd are linewise.
        let c = run("a\nb\nc\n", 0, &op("delete", 0, "", r#"(evil-motion "j")"#));
        assert!(kill(&c, 0, 4), "dj: {c:?}");
        assert!(c.iter().any(|c| matches!(c, C::EvilSetLinewise)), "{c:?}");
        let c = run(
            "a\nb\nc\nd\n",
            0,
            &op("delete", 3, "", r#"(evil-motion "line")"#),
        );
        assert!(kill(&c, 0, 6), "3dd: {c:?}");
        // d} from a paragraph's first column turns linewise (Vim's rule).
        let c = run(
            "aa\nbb\n\ncc\n",
            0,
            &op("delete", 0, "", r#"(evil-motion "}")"#),
        );
        assert!(kill(&c, 0, 6), "d}}: {c:?}");
        // f / t (inclusive, via the captured char), and the search motion.
        let c = run(
            "abcdef",
            0,
            &op("delete", 0, "", r#"(evil-motion-find "f" "d")"#),
        );
        assert!(kill(&c, 0, 4), "dfd: {c:?}");
        let c = run(
            "abcdef",
            0,
            &op("delete", 0, "", r#"(evil-motion-find "t" "d")"#),
        );
        assert!(kill(&c, 0, 3), "dtd: {c:?}");
        let c = run(
            "abcdef",
            5,
            &op("delete", 0, "", r#"(evil-motion-find "F" "b")"#),
        );
        assert!(kill(&c, 1, 5), "dFb: {c:?}");
        let c = run(
            "foo bar",
            0,
            &op("delete", 0, "", r#"(evil-motion-search "bar" #t)"#),
        );
        assert!(kill(&c, 0, 4), "d/bar: {c:?}");
        // Text objects, case operators, forced linewise.
        let c = run(
            "foo bar baz",
            5,
            &op("delete", 0, "", r#"(evil-object "i" "word")"#),
        );
        assert!(kill(&c, 4, 7), "diw: {c:?}");
        let c = run(
            "foo bar",
            0,
            &op("upcase", 0, "", r#"(evil-object "i" "word")"#),
        );
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { start: 0, end: 3, text } if text == "FOO")),
            "gUiw: {c:?}"
        );
        let c = run(
            "ab cd\nef\n",
            0,
            &op("delete", 0, "V", r#"(evil-motion "w")"#),
        );
        assert!(kill(&c, 0, 6), "dVw: {c:?}");
        // dvj: charwise, exclusive, keeping the column.
        let c = run(
            "abc\ndef\n",
            1,
            &op("delete", 0, "v", r#"(evil-motion "j")"#),
        );
        assert!(kill(&c, 1, 5), "dvj: {c:?}");
        // yy keeps the cursor; >j shifts two lines through the indent command.
        let c = run("a\nb\nc", 0, &op("yank", 2, "", r#"(evil-motion "line")"#));
        assert!(
            c.iter()
                .any(|c| matches!(c, C::SetRegion { start: 0, end: 4 })),
            "2yy: {c:?}"
        );
        let c = run("a\nb\nc\n", 0, &op("indent", 0, "", r#"(evil-motion "j")"#));
        assert!(c.iter().any(|c| matches!(c, C::IndentRegion)), ">j: {c:?}");
        // A failed motion (dh at column 0) does nothing — and c doesn't insert.
        let c = run("abc", 0, &op("change", 0, "", r#"(evil-motion "h")"#));
        assert!(
            !c.iter()
                .any(|c| matches!(c, C::EvilInsertState | C::KillRegionBetween { .. })),
            "ch at bol: {c:?}"
        );
        // dG / d3G — linewise to the last / third line.
        let c = run(
            "a\nb\nc\nd\n",
            0,
            &op("delete", 3, "", r#"(evil-motion "G")"#),
        );
        assert!(kill(&c, 0, 6), "d3G: {c:?}");
        // ) — sentence motion.
        let c = run(
            "One two. Three four.",
            0,
            &op("delete", 0, "", r#"(evil-motion ")")"#),
        );
        assert!(kill(&c, 0, 9), "d): {c:?}");
        // Visual-mode operator: gU on the selection, then back to Normal.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("abc def"),
            point: 2,
            region: Some((0, 3)),
            evil_state: "visual".into(),
            ..Default::default()
        });
        let c = engine
            .eval(r#"(evil-visual-operator "upcase")"#)
            .unwrap()
            .commands;
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { start: 0, end: 3, text } if text == "ABC")),
            "visual gU: {c:?}"
        );
        assert!(c.iter().any(|c| matches!(c, C::EvilNormalState)), "{c:?}");
    }

    /// Tree-sitter text objects and motions, the gc comment operator, ]d,
    /// and C-a / C-x — against real parse trees.
    #[test]
    fn evil_tree_sitter_objects_and_extras() {
        use EditorCommand as C;
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");
        let src = "fn alpha(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n// one\n// two\nstruct S {\n    x: i32,\n}\n\nfn beta() {}\n";
        let set = |lang: &str, text: &str, point: usize| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                point,
                selections: vec![(point, point)],
                language: lang.into(),
                major_mode: lang.into(),
                tree: crate::highlight::parse_for_test(lang, text),
                ..Default::default()
            });
        };
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap_or_default();
        let at = |needle: &str| src.find(needle).unwrap();
        // af: the whole function, linewise; if: the body lines.
        set("rust", src, at("a + b"));
        let fn_end = at("}\n\n// one") + 2;
        assert_eq!(
            val(r#"(evil-object-range-typed "a" "function")"#),
            format!("(\"line\" 0 {fn_end})")
        );
        assert_eq!(
            val(r#"(evil-object-range-typed "i" "function")"#),
            format!("(\"line\" {} {})", at("    a + b"), at("}\n\n// one"))
        );
        // aa / ia on a parameter: the last one takes the comma before it.
        set("rust", src, at("b: i32"));
        assert_eq!(
            val(r#"(evil-object-range-typed "i" "argument")"#),
            format!("(\"char\" {} {})", at("b: i32"), at("b: i32") + 6)
        );
        assert_eq!(
            val(r#"(evil-object-range-typed "a" "argument")"#),
            format!("(\"char\" {} {})", at(", b: i32"), at("b: i32") + 6)
        );
        set("rust", src, at("a: i32"));
        assert_eq!(
            val(r#"(evil-object-range-typed "a" "argument")"#),
            format!("(\"char\" {} {})", at("a: i32"), at("b: i32"))
        );
        // ac: the struct; a/: both adjacent line comments.
        set("rust", src, at("x: i32"));
        assert_eq!(
            val(r#"(evil-object-range-typed "a" "class")"#),
            format!("(\"line\" {} {})", at("struct S"), at("}\n\nfn beta") + 2)
        );
        set("rust", src, at("one"));
        assert_eq!(
            val(r#"(evil-object-range-typed "a" "comment")"#),
            format!("(\"line\" {} {})", at("// one"), at("struct S"))
        );
        // ]m from the top lands on beta; daf deletes alpha linewise.
        set("rust", src, 0);
        assert_eq!(
            val(r#"(evil-ts-motion-target "function" #t #f 1)"#),
            at("fn beta").to_string()
        );
        set("rust", src, at("a + b"));
        let c = engine
            .eval(r#"(evil-operate "delete" 0 "" (lambda () (evil-object "a" "function")))"#)
            .unwrap()
            .commands;
        assert!(
            c.iter()
                .any(|c| matches!(c, C::KillRegionBetween { start: 0, end } if *end == fn_end)),
            "daf: {c:?}"
        );
        // gc / gcc: comment at the shallowest indent, then uncomment.
        set("python", "a\n  b\n", 0);
        let c = engine.eval("(evil-comment-lines 0 6)").unwrap().commands;
        assert!(
            c.iter().any(
                |c| matches!(c, C::ReplaceRegion { start: 0, end: 5, text } if text == "# a\n#   b")
            ),
            "gc: {c:?}"
        );
        set("python", "# a\n  # b\n", 0);
        let c = engine.eval("(evil-comment-lines 0 10)").unwrap().commands;
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { text, .. } if text == "a\n  b")),
            "gc uncomment: {c:?}"
        );
        // C-a / C-x: decimal (signed), hex keeping width/case, x-1 is not -1.
        let inc = |text: &str, point: usize, delta: i64| {
            set("rust", text, point);
            engine
                .eval(&format!("(evil-increment-by {delta})"))
                .unwrap()
                .commands
        };
        let c = inc("x = 9;", 0, 1);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { start: 4, end: 5, text } if text == "10")),
            "{c:?}"
        );
        let c = inc("v -5 w", 0, 3);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { start: 2, end: 4, text } if text == "-2")),
            "{c:?}"
        );
        let c = inc("n = 0x0f", 0, 1);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { text, .. } if text == "0x10")),
            "{c:?}"
        );
        let c = inc("n = 0xFF", 0, 1);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { text, .. } if text == "0x100")),
            "{c:?}"
        );
        let c = inc("x-1", 0, 1);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::ReplaceRegion { start: 2, end: 3, text } if text == "2")),
            "{c:?}"
        );
        // ]d: the next diagnostic's position.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("aaa\nbbb\nccc\n"),
            point: 0,
            diagnostics: vec!["2\t1\terror\tboom".into(), "1\t0\twarning\tw".into()],
            ..Default::default()
        });
        let c = engine.eval("(evil-next-diagnostic)").unwrap().commands;
        assert!(c.iter().any(|c| matches!(c, C::GotoChar(4))), "]d: {c:?}");
    }

    /// More Scheme-side coverage for the late additions: Lisp function objects
    /// (by form head), visual C-a progressive, and the fold z-commands.
    #[test]
    fn evil_lisp_objects_visual_increment_and_folds() {
        use EditorCommand as C;
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        // af in Scheme: the enclosing (define …), not the inner (+ a b).
        let text = "(define (f a b)\n  (+ a b))\n";
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str(text),
            point: text.find("+ a").unwrap(),
            language: "scheme".into(),
            tree: crate::highlight::parse_for_test("scheme", text),
            ..Default::default()
        });
        assert_eq!(
            engine
                .eval(r#"(evil-object-range-typed "a" "function")"#)
                .unwrap()
                .value
                .as_deref(),
            Some(format!("(\"line\" 0 {})", text.len()).as_str())
        );
        // ia in Scheme: an operand, not the operator.
        assert_eq!(
            engine
                .eval(r#"(evil-object-range-typed "i" "argument")"#)
                .unwrap()
                .value
                .as_deref(),
            Some(
                format!(
                    "(\"char\" {} {})",
                    text.rfind("a b)").unwrap(),
                    text.rfind("a b)").unwrap() + 1
                )
                .as_str()
            )
        );
        // Visual g C-a over three lines: +1, +2, +3.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("x 0\nx 0\nx 0\n"),
            point: 0,
            region: Some((0, 11)),
            evil_state: "visual-line".into(),
            ..Default::default()
        });
        let c = engine
            .eval("(evil-visual-increment-progressive)")
            .unwrap()
            .commands;
        let reps: Vec<(usize, String)> = c
            .iter()
            .filter_map(|c| match c {
                C::ReplaceRegion { start, text, .. } => Some((*start, text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            reps,
            vec![
                (10, "3".to_string()),
                (6, "2".to_string()),
                (2, "1".to_string())
            ],
            "{c:?}"
        );
        // zc inside an indented block closes its header's fold.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("a:\n  b\n  c\nd\n"),
            point: 5,
            line: 1,
            name: "f.py".into(),
            ..Default::default()
        });
        let c = engine.eval("(fold-close-at-point)").unwrap().commands;
        assert!(
            c.iter().any(|c| matches!(c, C::FoldLines { .. })),
            "zc should fold the block: {c:?}"
        );
        // `r` resolves to a replayable, count-aware command.
        assert!(matches!(
            engine.eval(r#"(evil-replace-with "x")"#).unwrap().commands.as_slice(),
            [C::EvilReplaceWith(ch)] if ch == "x"
        ));
    }

    #[test]
    fn evil_leader_key_configurable() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");

        // Default leader is SPC: a global leader binding lands under "space …".
        let cmds = engine
            .eval(r#"(evil-leader-set "n n" "next-buffer")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::DefineKey(m, s, cmd)
                if m == "evil-normal" && s == "space n n" && cmd == "next-buffer")),
            "default leader should expand to 'space n n': {cmds:?}"
        );
        // …and the same chord lands in the "evil-leader" map (special buffers,
        // terminals, Insert) under both the leader and the alt leader.
        for (map, seq) in [
            ("evil-leader", "space n n"),
            ("evil-leader", "alt-space n n"),
            ("evil-normal", "alt-space n n"),
        ] {
            assert!(
                cmds.iter()
                    .any(|c| matches!(c, EditorCommand::DefineKey(m, s, cmd)
                    if m == map && s == seq && cmd == "next-buffer")),
                "expected {seq} in {map}: {cmds:?}"
            );
        }

        // Disabling the alt leader drops the alt sequences.
        engine.eval(r#"(evil-set-leader-alt "")"#).expect("alt off");
        let cmds = engine
            .eval(r#"(evil-leader-set "n p" "previous-buffer")"#)
            .unwrap()
            .commands;
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::DefineKey(_, s, _)
                if s.starts_with("alt-space"))),
            "no alt-leader bindings once disabled: {cmds:?}"
        );

        // Changing the leader moves subsequent bindings to the new prefix; a
        // per-mode leader binding scopes to "evil-normal:<mode>".
        engine
            .eval(r#"(evil-set-leader "comma")"#)
            .expect("set leader");
        let cmds = engine
            .eval(
                r#"(evil-leader-set-for-modes '("rust")
                     (list (cons "g r" "lsp-rename")))"#,
            )
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::DefineKey(m, s, cmd)
                if m == "evil-normal:rust" && s == "comma g r" && cmd == "lsp-rename")),
            "reconfigured leader should expand to 'comma g r' in rust map: {cmds:?}"
        );
    }

    #[test]
    fn evil_collection_overlays_special_modes() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // evil.scm calls `(evil-collection-init)` at load, populating the
        // `evil-normal:<mode>` overlay keymaps for every special mode.
        let cmds = engine
            .eval(include_str!("../../scheme/evil.scm"))
            .unwrap()
            .commands;
        for (mode, key, cmd) in [
            ("evil-normal:magit-status", "j", "next-line"),
            ("evil-normal:magit-status", "k", "previous-line"),
            ("evil-normal:dired-mode", "k", "previous-line"),
        ] {
            assert!(
                cmds.iter()
                    .any(|c| matches!(c, EditorCommand::DefineKey(m, s, x)
                    if m == mode && s == key && x == cmd)),
                "evil-collection-init should bind {key}->{cmd} in {mode}"
            );
        }

        // The overlay is configurable from Scheme — extend the mode list and
        // re-run, and a newly added mode gets the same motion.
        let cmds = engine
            .eval(
                r#"(begin
                     (set! *evil-collection-modes* (cons "my-mode" *evil-collection-modes*))
                     (evil-collection-init))"#,
            )
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::DefineKey(m, s, x)
                if m == "evil-normal:my-mode" && s == "j" && x == "next-line")),
            "re-running evil-collection-init should cover newly added modes"
        );
    }

    #[test]
    fn isearch_commands_dispatch() {
        let (engine, _rx) = ScriptEngine::new();
        assert!(matches!(
            engine
                .eval("(isearch-forward)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::IsearchForward]
        ));
        assert!(matches!(
            engine
                .eval("(isearch-backward)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::IsearchBackward]
        ));
    }

    #[test]
    fn tutorial_loads_and_opens() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // The whole tutorial buffer text must parse as a Steel string literal.
        engine
            .eval(include_str!("../../scheme/tutorial.scm"))
            .expect("tutorial.scm");
        // help-with-tutorial should build the *TUTORIAL* buffer and move point.
        let cmds = engine.eval("(help-with-tutorial)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::AppendToBuffer { name, .. } if name == "*TUTORIAL*")
            ),
            "help-with-tutorial should append to *TUTORIAL*: {cmds:?}"
        );
    }

    #[test]
    fn version_command_echoes_package_version() {
        let (engine, _rx) = ScriptEngine::new();
        // The `version` command's body (help.scm) over the `macros-version`
        // builtin echoes "Macros <pkg-version>" via the message command.
        let expected = format!("Macros {}", env!("CARGO_PKG_VERSION"));
        assert!(
            matches!(
                engine
                    .eval(r#"(message (string-append "Macros " (macros-version)))"#)
                    .unwrap()
                    .commands
                    .as_slice(),
                [EditorCommand::Message(m)] if *m == expected
            ),
            "version should echo {expected:?}"
        );
    }

    #[test]
    fn eval_collecting_keeps_commands_before_a_runtime_error() {
        let (engine, _rx) = ScriptEngine::new();
        // A *runtime* error in a later form must NOT discard the commands that
        // already ran — otherwise a bad call near the bottom of init.scm would
        // drop the `(evil-mode)` near the top, leaving the user in plain
        // (non-modal) editing. (Steel compiles the whole file first, so
        // compile-time errors — unbound symbols, syntax — still abort everything;
        // that's inherent and can't be salvaged here.)
        let (commands, error) = engine.eval_collecting("(evil-mode) (car '())");
        assert!(
            matches!(commands.as_slice(), [EditorCommand::EvilMode]),
            "got {commands:?}"
        );
        assert!(error.is_some(), "car of '() should surface a runtime error");
        // A clean program reports no error and still returns its commands.
        let (ok_cmds, ok_err) = engine.eval_collecting("(evil-mode)");
        assert!(matches!(ok_cmds.as_slice(), [EditorCommand::EvilMode]));
        assert!(ok_err.is_none());
    }

    #[test]
    fn negative_offsets_never_reach_the_editor() {
        // Steel wraps a negative int converted to `usize` (-1 → usize::MAX),
        // which the editor then clamped to the buffer END — `(delete-region -5
        // 2)` deleted from 2 to the end. Position primitives clamp negatives
        // to 0 at the boundary instead.
        let (engine, _rx) = ScriptEngine::new();
        let (commands, error) = engine.eval_collecting(
            "(goto-char -1) (delete-region -5 2) (replace-region 0 -1 \"x\") \
             (insert-at -1 \"x\") (set-region -1 3) (select-to -2)",
        );
        assert!(error.is_none(), "{error:?}");
        assert!(
            matches!(
                commands.as_slice(),
                [
                    EditorCommand::GotoChar(0),
                    EditorCommand::DeleteRegion { start: 0, end: 2 },
                    EditorCommand::ReplaceRegion {
                        start: 0,
                        end: 0,
                        ..
                    },
                    EditorCommand::InsertAt { pos: 0, .. },
                    EditorCommand::SetRegion { start: 0, end: 3 },
                    EditorCommand::SelectTo(0),
                ]
            ),
            "got {commands:?}"
        );
        // Pure reads clamp the same way (no snapshot published → empty text).
        assert_eq!(
            engine
                .eval("(buffer-substring -3 2)")
                .unwrap()
                .value
                .as_deref(),
            Some("\"\"")
        );
        assert_eq!(
            engine
                .eval("(grapheme-backward -1)")
                .unwrap()
                .value
                .as_deref(),
            Some("0")
        );
    }

    #[test]
    fn evil_visual_block_commands_resolve() {
        let (engine, _rx) = ScriptEngine::new();
        // `C-v` enters block-wise Visual.
        assert!(matches!(
            engine
                .eval("(evil-visual-block-state)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilVisualBlockState]
        ));
        // The block operators: insert/append down a column, delete, yank.
        assert!(matches!(
            engine
                .eval("(evil-block-insert)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilBlockInsert]
        ));
        assert!(matches!(
            engine
                .eval("(evil-block-append)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilBlockAppend]
        ));
        assert!(matches!(
            engine
                .eval("(evil-block-delete)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilBlockDelete]
        ));
        assert!(matches!(
            engine
                .eval("(evil-block-yank)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilBlockYank]
        ));
    }

    #[test]
    fn evil_state_and_composite_operators() {
        let (engine, _rx) = ScriptEngine::new();
        // State commands enqueue.
        assert!(matches!(
            engine.eval("(evil-mode)").unwrap().commands.as_slice(),
            [EditorCommand::EvilMode]
        ));
        // The composite-operator pattern evil.scm uses: one Scheme fn enqueues
        // several commands, applied in order (here: `a` = append).
        engine
            .eval("(define (evil-append) (forward-char) (evil-insert-state))")
            .unwrap();
        assert!(matches!(
            engine.eval("(evil-append)").unwrap().commands.as_slice(),
            [EditorCommand::ForwardChar, EditorCommand::EvilInsertState]
        ));
    }

    /// Registers and register macros: `"x`, `C-r x`, `qx` / `q`, `@x` reach
    /// their editor commands.
    #[test]
    fn evil_registers_and_macro_commands() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm");
        let c = engine.eval(r#"(evil-use-register "a")"#).unwrap().commands;
        assert!(matches!(c.as_slice(), [EditorCommand::EvilUseRegister(r)] if r == "a"));
        let c = engine.eval(r#"(evil-macro-play "q")"#).unwrap().commands;
        assert!(matches!(c.as_slice(), [EditorCommand::EvilMacroPlay(r)] if r == "q"));
        // `q` arms a register read when idle, and stops a recording otherwise.
        engine.set_snapshot(EditorSnapshot::default());
        let c = engine.eval("(evil-kbd-macro-toggle)").unwrap().commands;
        assert!(
            c.iter().any(|c| matches!(c, EditorCommand::ReadChar { callback, .. } if callback == "evil-macro-record")),
            "{c:?}"
        );
        engine.set_snapshot(EditorSnapshot {
            kbd_macro_recording: true,
            ..Default::default()
        });
        let c = engine.eval("(evil-kbd-macro-toggle)").unwrap().commands;
        assert!(
            matches!(c.as_slice(), [EditorCommand::EndKbdMacro]),
            "{c:?}"
        );
    }

    /// Marks: bindings, the mark motions under an operator (`d'a` linewise,
    /// `` d`a `` exclusive), and the jump-command registry.
    #[test]
    fn evil_marks_and_jump_commands() {
        use EditorCommand as C;
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        let cmds = engine
            .eval(include_str!("../../scheme/evil.scm"))
            .expect("evil.scm")
            .commands;
        let bound = |mode: &str, chord: &str, to: &str| {
            cmds.iter().any(
                |c| matches!(c, C::DefineKey(m, s, cmd) if m == mode && s == chord && cmd == to),
            )
        };
        assert!(bound("evil-normal", "m", "evil-set-mark %c"));
        assert!(bound("evil-normal", "'", "evil-goto-mark-line %c"));
        assert!(bound("evil-normal", "`", "evil-goto-mark %c"));
        assert!(bound("evil-operator", "'", "evil-motion-mark %c #f"));
        assert!(bound("evil-normal", "g v", "evil-reselect-visual"));
        // The jump list: C-o back, C-i forward (TAB falls back to it in Rust).
        assert!(bound("evil-normal", "ctrl-o", "evil-jump-backward"));
        assert!(bound("evil-normal", "ctrl-i", "evil-jump-forward"));
        // C-w window commands.
        assert!(bound("evil-normal", "ctrl-w h", "windmove-left"));
        assert!(bound("evil-normal", "ctrl-w ctrl-l", "windmove-right"));
        assert!(bound("evil-normal", "ctrl-w =", "balance-windows"));
        assert!(bound(
            "evil-normal",
            "ctrl-w shift-k",
            "evil-window-move-very-top"
        ));
        assert!(bound("evil-normal", "ctrl-w o", "delete-other-windows"));
        // Ex: & / g& repeat the last :s; `:` from Visual.
        assert!(bound("evil-normal", "&", r#"evil-ex "s""#));
        assert!(bound("evil-normal", "g &", r#"evil-ex "%&&""#));
        assert!(bound("evil-visual-line", ":", "evil-ex-command"));
        // Tree-sitter motions / objects, diagnostics, folds, C-a, gc.
        assert!(bound("evil-normal", "] m", "evil-next-function-start"));
        assert!(bound("evil-normal", "[ [", "evil-previous-class-start"));
        assert!(bound(
            "evil-operator",
            "] m",
            r#"evil-ts-motion "function" #t #f"#
        ));
        assert!(bound(
            "evil-operator",
            "a f",
            r#"evil-object "a" "function""#
        ));
        assert!(bound(
            "evil-visual",
            "i a",
            r#"evil-object-cmd "select" "i" "argument""#
        ));
        assert!(bound("evil-normal", "] d", "evil-next-diagnostic"));
        assert!(bound("evil-normal", "z c", "fold-close-at-point"));
        assert!(bound("evil-normal", "z shift-m", "fold-all"));
        assert!(bound("evil-normal", "ctrl-a", "evil-increment"));
        assert!(bound(
            "evil-visual",
            "g ctrl-a",
            "evil-visual-increment-progressive"
        ));
        assert!(bound("evil-normal", "g c", r#"evil-operator "comment""#));
        for jump in [
            "evil-goto-last-line",
            "evil-search-next",
            "evil-matchit",
            "lsp-find-definition",
        ] {
            assert!(
                cmds.iter()
                    .any(|c| matches!(c, C::SetJumpCommand(n) if n == jump)),
                "{jump} should be a jump command"
            );
        }
        let run = |src: &str| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str("aa\nbb\ncc\ndd\n"),
                point: 1,
                marks: vec![("a".into(), 7)],
                ..Default::default()
            });
            engine.eval(src).unwrap().commands
        };
        let c = run(r#"(evil-operate "delete" 0 "" (lambda () (evil-motion-mark "a" #f)))"#);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::KillRegionBetween { start: 0, end: 9 })),
            "d'a: {c:?}"
        );
        let c = run(r#"(evil-operate "delete" 0 "" (lambda () (evil-motion-mark "a" #t)))"#);
        assert!(
            c.iter()
                .any(|c| matches!(c, C::KillRegionBetween { start: 1, end: 7 })),
            "d`a: {c:?}"
        );
        // An unset mark fails the motion: nothing happens.
        let c = run(r#"(evil-operate "delete" 0 "" (lambda () (evil-motion-mark "b" #t)))"#);
        assert!(
            !c.iter().any(|c| matches!(c, C::KillRegionBetween { .. })),
            "{c:?}"
        );
    }

    #[test]
    fn evil_visual_line_commands_resolve() {
        let (engine, _rx) = ScriptEngine::new();
        // `V` enters line-wise Visual.
        assert!(matches!(
            engine
                .eval("(evil-visual-line-state)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilVisualLineState]
        ));
        // Blockwise insert (`I`) and append (`A`) over the selection.
        assert!(matches!(
            engine
                .eval("(evil-visual-insert)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilVisualInsert]
        ));
        assert!(matches!(
            engine
                .eval("(evil-visual-append)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EvilVisualAppend]
        ));
        // The line-wise delete/yank pattern from evil.scm: kill, mark line-wise,
        // return to Normal (so a following `p` pastes the vim way).
        engine
            .eval(
                "(define (evil-visual-line-delete) \
                   (kill-region) (evil-set-linewise) (evil-normal-state))",
            )
            .unwrap();
        assert!(matches!(
            engine
                .eval("(evil-visual-line-delete)")
                .unwrap()
                .commands
                .as_slice(),
            [
                EditorCommand::KillRegion,
                EditorCommand::EvilSetLinewise,
                EditorCommand::EvilNormalState
            ]
        ));
    }

    #[test]
    fn buffer_list_bridge_and_helm_buffers_app() {
        let (engine, _rx) = ScriptEngine::new();
        engine.set_buffer_names(vec!["*scratch*".to_string(), "main.scm".to_string()]);

        // Scheme can read the live buffer list.
        assert_eq!(
            engine
                .eval("(length (buffer-names))")
                .unwrap()
                .value
                .as_deref(),
            Some("2")
        );

        // `switch-to-buffer` is a command taking the chosen name.
        let switched = engine.eval(r#"(switch-to-buffer "main.scm")"#).unwrap();
        assert!(matches!(
            switched.commands.as_slice(),
            [EditorCommand::SwitchToBufferNamed(n)] if n == "main.scm"
        ));

        // The whole `helm-buffers` app (verbatim from helm.scm) composes the two
        // primitives into a Helm session — fully in Scheme.
        engine
            .eval(
                r#"(define (helm-buffers)
                     (helm (list (list "Buffers" (buffer-names) "switch-to-buffer"))))"#,
            )
            .unwrap();
        match engine.eval("(helm-buffers)").unwrap().commands.as_slice() {
            [EditorCommand::Helm(sources)] => {
                assert_eq!(sources[0].name, "Buffers");
                assert!(matches!(
                    &sources[0].candidates,
                    HelmCandidates::Static(c)
                        if c == &[("*scratch*".to_string(), None), ("main.scm".to_string(), None)]
                ));
                assert_eq!(
                    sources[0].actions,
                    [("default".to_string(), "switch-to-buffer".to_string())]
                );
            }
            other => panic!("expected one Helm command, got {other:?}"),
        }
    }

    #[test]
    fn buffer_file_name_of_bridge() {
        let (engine, _rx) = ScriptEngine::new();
        engine.set_buffer_paths(HashMap::from([(
            "main.rs".to_string(),
            "/proj/src/main.rs".to_string(),
        )]));

        // A file-visiting buffer resolves to its absolute path.
        assert_eq!(
            engine
                .eval(r#"(buffer-file-name-of "main.rs")"#)
                .unwrap()
                .value
                .as_deref(),
            Some(r#""/proj/src/main.rs""#)
        );
        // An unknown / fileless buffer yields the empty string.
        assert_eq!(
            engine
                .eval(r#"(buffer-file-name-of "*scratch*")"#)
                .unwrap()
                .value
                .as_deref(),
            Some(r#""""#)
        );
    }

    #[test]
    fn call_action_runs_named_function_with_candidate() {
        let (engine, _rx) = ScriptEngine::new();
        // An action is just a named Steel function taking the chosen candidate.
        engine.eval("(define (act x) (message x))").unwrap();
        let commands = engine.call_action("act", Some("chosen"));
        assert!(matches!(commands.as_slice(), [EditorCommand::Message(m)] if m == "chosen"));
    }

    #[test]
    fn dispatch_delivers_outcome_off_thread() {
        use futures::StreamExt as _;
        let (engine, mut outcomes) = ScriptEngine::new();
        // Async dispatch: the engine runs on its thread and the outcome arrives
        // on the channel the editor drains (no UI-thread blocking).
        engine.dispatch(r#"(message "hi")"#, false);
        match futures::executor::block_on(outcomes.next()).unwrap() {
            ScriptOutcome::Commands { commands, message } => {
                assert!(matches!(commands.as_slice(), [EditorCommand::Message(m)] if m == "hi"));
                assert!(message.is_none());
            }
            _ => panic!("expected Commands"),
        }

        // Interactive dispatch carries the formatted value as the echo message.
        engine.dispatch("(+ 1 2)", true);
        match futures::executor::block_on(outcomes.next()).unwrap() {
            ScriptOutcome::Commands { message, .. } => assert_eq!(message.as_deref(), Some("=> 3")),
            _ => panic!("expected Commands"),
        }
    }

    #[test]
    fn dynamic_candidates_dispatch() {
        use futures::StreamExt as _;
        let (engine, mut outcomes) = ScriptEngine::new();
        engine
            .eval(r#"(define (cands p) (list (string-append p "x") "static"))"#)
            .unwrap();
        engine.dispatch_candidates(2, 7, "cands", "pre");
        match futures::executor::block_on(outcomes.next()).unwrap() {
            ScriptOutcome::Candidates {
                source,
                query_id,
                candidates,
            } => {
                assert_eq!(source, 2);
                assert_eq!(query_id, 7);
                assert_eq!(
                    candidates,
                    vec![("prex".to_string(), None), ("static".to_string(), None)]
                );
            }
            _ => panic!("expected Candidates"),
        }
    }

    #[test]
    fn dynamic_candidates_carry_annotations() {
        use futures::StreamExt as _;
        let (engine, mut outcomes) = ScriptEngine::new();
        // A candidate may be a bare string or a (candidate annotation) pair.
        engine
            .eval(r#"(define (cands p) (list (list "switch-to-buffer" "C-x b") "plain"))"#)
            .unwrap();
        engine.dispatch_candidates(0, 1, "cands", "");
        match futures::executor::block_on(outcomes.next()).unwrap() {
            ScriptOutcome::Candidates { candidates, .. } => {
                assert_eq!(
                    candidates,
                    vec![
                        ("switch-to-buffer".to_string(), Some("C-x b".to_string())),
                        ("plain".to_string(), None),
                    ]
                );
            }
            _ => panic!("expected Candidates"),
        }
    }

    #[test]
    fn capf_dispatch_returns_items_and_range() {
        use futures::StreamExt as _;
        let (engine, mut outcomes) = ScriptEngine::new();
        // A CAPF returning (start end candidate-list), candidates with annotations.
        engine
            .eval(
                r#"(define (my-capf prefix)
                     (list 3 7 (list (list "lambda" "kw") "let")))"#,
            )
            .unwrap();
        engine.dispatch_capf(vec!["my-capf".to_string()], "le", 3, 7);
        match futures::executor::block_on(outcomes.next()).unwrap() {
            ScriptOutcome::Capf { start, end, items } => {
                assert_eq!((start, end), (3, 7));
                assert_eq!(
                    items,
                    vec![
                        ("lambda".to_string(), Some("kw".to_string())),
                        ("let".to_string(), None),
                    ]
                );
            }
            _ => panic!("expected Capf"),
        }
    }

    #[test]
    fn capf_falls_through_to_next_function() {
        use futures::StreamExt as _;
        let (engine, mut outcomes) = ScriptEngine::new();
        // The first CAPF declines (empty); the second supplies a flat list and
        // keeps the default byte range.
        engine.eval(r"(define (capf-a prefix) '())").unwrap();
        engine
            .eval(r#"(define (capf-b prefix) (list "alpha" "beta"))"#)
            .unwrap();
        engine.dispatch_capf(vec!["capf-a".to_string(), "capf-b".to_string()], "a", 2, 5);
        match futures::executor::block_on(outcomes.next()).unwrap() {
            ScriptOutcome::Capf { start, end, items } => {
                assert_eq!((start, end), (2, 5));
                assert_eq!(
                    items,
                    vec![("alpha".to_string(), None), ("beta".to_string(), None)]
                );
            }
            _ => panic!("expected Capf"),
        }
    }

    #[test]
    fn builtin_fast_path_recognises_nullary_calls() {
        let (engine, _outcomes) = ScriptEngine::new();
        assert!(matches!(
            engine.builtins_for("(forward-char)").as_deref(),
            Some([EditorCommand::ForwardChar])
        ));
        assert!(matches!(
            engine.builtins_for("  (undo)  ").as_deref(),
            Some([EditorCommand::Undo])
        ));
        // A sequence of bare builtins is fast-pathed in order.
        assert!(matches!(
            engine.builtins_for("(forward-char) (undo)").as_deref(),
            Some([EditorCommand::ForwardChar, EditorCommand::Undo])
        ));
        // User functions / argument calls are NOT fast-pathed (go to the thread).
        assert!(engine.builtins_for("(my-fn)").is_none());
        assert!(engine.builtins_for(r#"(insert "x")"#).is_none());
        // A non-builtin anywhere in the sequence drops the whole thing.
        assert!(engine.builtins_for("(forward-char) (my-fn)").is_none());
        assert!(engine.builtins_for("").is_none());
    }

    #[test]
    fn eval_returns_value_and_commands() {
        let (engine, _rx) = ScriptEngine::new();

        // A pure expression yields a printable value, no commands.
        let result = engine.eval("(+ 1 2)").unwrap();
        assert_eq!(result.value.as_deref(), Some("3"));
        assert!(result.commands.is_empty());

        // A command call enqueues it and yields a void (no) value.
        let result = engine.eval(r#"(message "hi")"#).unwrap();
        assert!(matches!(result.commands.as_slice(), [EditorCommand::Message(m)] if m == "hi"));
        assert_eq!(result.value, None);
    }

    #[test]
    fn set_auto_update_maps_to_option() {
        let (engine, _rx) = ScriptEngine::new();
        // Mirrors the bundled `set-auto-update` in core.scm: a thin wrapper that
        // flips the "auto-update" option (now passed as a native boolean).
        use crate::settings::OptionValue;
        let def = r#"(define (set-auto-update enable)
                       (set-option "auto-update" enable))"#;

        let result = engine.eval(&format!("{def} (set-auto-update #f)")).unwrap();
        assert!(matches!(
            result.commands.as_slice(),
            [EditorCommand::SetOption(k, v)] if k == "auto-update" && matches!(v, OptionValue::Bool(false))
        ));

        let result = engine.eval(&format!("{def} (set-auto-update #t)")).unwrap();
        assert!(matches!(
            result.commands.as_slice(),
            [EditorCommand::SetOption(k, v)] if k == "auto-update" && matches!(v, OptionValue::Bool(true))
        ));
    }

    #[test]
    fn get_option_reads_published_values() {
        use crate::settings::OptionValue;
        let (engine, _rx) = ScriptEngine::new();
        let mut opts = std::collections::HashMap::new();
        opts.insert("font-size".to_string(), OptionValue::Int(13));
        opts.insert("completion-auto".to_string(), OptionValue::Bool(false));
        opts.insert(
            "cursor-style".to_string(),
            OptionValue::Str("bar".to_string()),
        );
        engine.set_options(opts);

        let v = |src: &str| engine.eval(src).unwrap().value;
        assert_eq!(v(r#"(get-option "font-size")"#).as_deref(), Some("13"));
        assert_eq!(
            v(r#"(get-option "completion-auto")"#).as_deref(),
            Some("#false")
        );
        assert_eq!(
            v(r#"(get-option "cursor-style")"#).as_deref(),
            Some("\"bar\"")
        );
        // An unset option reads as #f.
        assert_eq!(v(r#"(get-option "nope")"#).as_deref(), Some("#false"));
    }

    #[test]
    fn bind_key_accepts_string_and_procedure() {
        let (engine, _rx) = ScriptEngine::new();
        // bind-key lives in the bundled core.scm — load the standard library.
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }

        // A string command behaves exactly like define-key.
        let r = engine
            .eval(r#"(bind-key "fundamental" "ctrl-x g" "magit-status")"#)
            .unwrap();
        assert!(matches!(
            r.commands.as_slice(),
            [EditorCommand::DefineKey(m, c, cmd)]
                if m == "fundamental" && c == "ctrl-x g" && cmd == "magit-status"
        ));

        // The canonical 'name symbol resolves to the same define-key path.
        let r = engine
            .eval(r#"(bind-key "fundamental" "ctrl-x s" 'magit-status)"#)
            .unwrap();
        assert!(matches!(
            r.commands.as_slice(),
            [EditorCommand::DefineKey(m, c, cmd)]
                if m == "fundamental" && c == "ctrl-x s" && cmd == "magit-status"
        ));

        // A procedure is stashed and the chord is bound to a run-thunk stub.
        engine
            .eval("(define *fired* #f) (define (mark!) (set! *fired* #t))")
            .unwrap();
        let r = engine
            .eval(r#"(bind-key "fundamental" "ctrl-x h" mark!)"#)
            .unwrap();
        let stub = match r.commands.as_slice() {
            [EditorCommand::DefineKey(_, _, cmd)] => cmd.clone(),
            other => panic!("expected one DefineKey, got {other:?}"),
        };
        assert!(stub.starts_with("run-thunk \"thunk-"), "stub was {stub:?}");

        // Firing the stub (as key dispatch does) runs the procedure.
        engine.eval(&format!("({stub})")).unwrap();
        assert_eq!(
            engine.eval("*fired*").unwrap().value.as_deref(),
            Some("#true")
        );
    }

    /// `position->byte` maps an LSP (line, UTF-16 column) position to a byte
    /// offset, clamping a past-the-end column to the line's newline and a
    /// past-the-end line to the end of the buffer — multibyte-aware on both
    /// axes (UTF-8 bytes vs UTF-16 columns).
    #[test]
    fn position_to_byte_resolves_lsp_positions() {
        let (engine, _rx) = ScriptEngine::new();
        // "aé\n𐍈x\n" — é: 2 bytes / 1 UTF-16 unit; 𐍈: 4 bytes / 2 UTF-16 units.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("aé\n𐍈x\n"),
            ..Default::default()
        });
        let eval = |src: &str| engine.eval(src).unwrap().value.unwrap();
        assert_eq!(eval("(position->byte 0 0)"), "0");
        assert_eq!(eval("(position->byte 0 1)"), "1");
        assert_eq!(eval("(position->byte 0 2)"), "3"); // é is 2 bytes wide
        assert_eq!(eval("(position->byte 0 99)"), "3"); // clamps at the newline
        assert_eq!(eval("(position->byte 1 0)"), "4");
        assert_eq!(eval("(position->byte 1 2)"), "8"); // 𐍈: 2 UTF-16 units, 4 bytes
        assert_eq!(eval("(position->byte 1 3)"), "9");
        assert_eq!(eval("(position->byte 99 0)"), "10"); // past-the-end line
    }
}

#[cfg(test)]
mod magit_tests {
    use super::*;

    /// Every embedded startup script loads, in real startup order, through one
    /// engine — catches cross-file identifier/ordering bugs and missing bridges.
    #[test]
    fn all_startup_scripts_load() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        // Every user-facing entry point is M-x discoverable (registered for the
        // helm-execute-command list). Guards against a lib forgetting to register.
        for cmd in [
            "magit-status",
            "magit",
            "dired",
            "shell",
            "compile",
            "occur",
            "comment-dwim",
            "replace-string",
            "helm-buffers",
            "helm-find-files",
            "helm-magit",
            "projectile-find-file",
            "projectile-grep",
            "projectile-switch-project",
            "sort-lines",
            "goto-line-cmd",
            "fill-paragraph",
            "set-fill-column",
            "licenses",
        ] {
            let q = format!(r#"(if (member "{cmd}" *interactive-commands*) "y" "n")"#);
            assert_eq!(
                engine.eval(&q).unwrap().value.as_deref(),
                Some("\"y\""),
                "{cmd} is not registered for M-x"
            );
        }
    }

    /// END-TO-END over the real startup load: switching perspectives (as the
    /// title-bar dropdown does, via perspective-open) repoints BOTH consumers —
    /// core.scm's `project-directory` (compile, term) and magit.scm's
    /// `magit-current-repo` (`C-x g`). This must run through the actual
    /// cross-file wiring: perspectives.scm cannot install the hooks with a raw
    /// top-level `set!` (a later file's top-level set! doesn't mutate the
    /// binding an earlier file's functions read — Steel resolves free
    /// identifiers at definition time); it must call the owner files' setter
    /// functions, whose internal set! mutates the binding the owners read.
    #[test]
    fn perspective_switch_repoints_project_directory_and_magit() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // This repo is a real project + git work-tree — a root the hooks can
        // resolve without any editor/buffer state behind the snapshot.
        let root = env!("CARGO_MANIFEST_DIR");
        engine
            .eval(&format!(
                r#"(perspective-update "{root}" "{root}/Cargo.toml")"#
            ))
            .unwrap();
        engine
            .eval(&format!(r#"(perspective-open "{root}")"#))
            .unwrap();
        // No file-visiting buffer exists (empty snapshot), so both results can
        // only come from the perspective hooks — the buffer fallbacks yield #f.
        assert_eq!(
            v("(project-directory)"),
            format!("\"{root}\""),
            "compile/term should target the selected perspective"
        );
        assert_eq!(
            v("(magit-current-repo)"),
            format!("\"{root}\""),
            "magit should target the selected perspective"
        );
    }

    /// PRODUCTION-FAITHFUL repro of "first compile after a title-bar switch runs
    /// in the old project": every step goes through the editor's real dispatch
    /// (`call_action` = Task::Call, exactly what dropdown clicks, hooks, and
    /// minibuffer callbacks use — NOT Task::Eval), with the snapshot updated the
    /// way the editor publishes it around each step. Two real on-disk projects;
    /// switch A→B via the dropdown's `perspective-select`, simulate the resume
    /// file opening (snapshot → B, find-file-hook), then run compile ONCE — it
    /// must cd into B immediately, not on the second try.
    ///
    /// Flat harness with a stubbed persistence-dir, NOT the full startup set:
    /// perspective-track calls perspective-save, and under the real startup
    /// scripts that writes to the developer's live ~/.config/macros/perspectives.
    /// The per-file eval structure (the binding semantics under test) is the
    /// same either way.
    #[test]
    fn dropdown_switch_then_first_compile_uses_new_project() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/diagnostics.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        engine.eval(include_str!("../../scheme/helm.scm")).unwrap();
        engine
            .eval(&format!(
                r#"
                (define (persistence-dir) "{}")
                (define *magit-scope-hook* #f)
                (define (set-magit-scope-hook! f) (set! *magit-scope-hook* f))
                "#,
                fresh_persistence_dir()
            ))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/titlebar-menu.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/perspectives.scm"))
            .unwrap();
        engine.eval(include_str!("../../scheme/shell.scm")).unwrap();
        // Two throwaway projects with distinct basenames (so dropdown labels are
        // their basenames) and a marker file each.
        let base = std::env::temp_dir().join("macros-persp-switch-test");
        let (proj_a, proj_b) = (base.join("alpha"), base.join("beta"));
        for p in [&proj_a, &proj_b] {
            std::fs::create_dir_all(p).unwrap();
            std::fs::write(p.join(".projectile"), "").unwrap();
            std::fs::write(p.join("main.txt"), "x\n").unwrap();
        }
        let (a, b) = (proj_a.to_str().unwrap(), proj_b.to_str().unwrap());
        let snap = |dir: &str, file: &str| {
            engine.set_snapshot(EditorSnapshot {
                name: "main.txt".to_string(),
                directory: dir.to_string(),
                path: file.to_string(),
                ..Default::default()
            });
        };
        // Work in A, then in B, then back in A — as find-file-hook would fire.
        snap(a, &format!("{a}/main.txt"));
        engine.call_action("perspective-track", None);
        snap(b, &format!("{b}/main.txt"));
        engine.call_action("perspective-track", None);
        snap(a, &format!("{a}/main.txt"));
        engine.call_action("perspective-track", None);
        // Title-bar switch to beta: the dropdown click's exact dispatch.
        engine.call_action("perspective-select", Some("beta"));
        // The editor processes the queued open-file: snapshot now describes B's
        // resume file and find-file-hook fires perspective-track.
        snap(b, &format!("{b}/main.txt"));
        engine.call_action("perspective-track", None);
        let compile_cd = |engine: &ScriptEngine| {
            let cmds = engine.call_action("compile-run", Some("make"));
            cmds.iter()
                .find_map(|c| match c {
                    EditorCommand::MakeProcess(p) => {
                        p.args.iter().find(|s| s.contains("cd ")).cloned()
                    }
                    _ => None,
                })
                .expect("compile should spawn a process")
        };
        // FIRST compile after the switch — regression: it ran in alpha.
        let cd = compile_cd(&engine);
        assert!(
            cd.contains(&format!("cd '{b}' && make")),
            "first compile after switch must run in beta, ran: {cd}"
        );
        engine.call_action("compile-done", Some("0"));
        // An INCIDENTAL activation of an alpha buffer — killing *compilation*
        // activates the ring's survivor, pane dances re-select windows — fires
        // buffer-switch-hook (perspective-note). It must do resume bookkeeping
        // only, NOT re-point the current perspective: the user selected beta.
        // Regression: the next compile silently ran in alpha.
        snap(a, &format!("{a}/main.txt"));
        engine.call_action("perspective-note", None);
        assert_eq!(
            engine.eval("*perspective-current*").unwrap().value.unwrap(),
            format!("\"{b}\""),
            "incidental buffer activation must not switch the perspective"
        );
        let cd = compile_cd(&engine);
        assert!(
            cd.contains(&format!("cd '{b}' && make")),
            "second compile must still run in beta, ran: {cd}"
        );
    }

    /// Deliberately switching to a file in another project (C-x b / cycling /
    /// revisiting an open buffer / an LSP jump) fires `buffer-select-hook`,
    /// which re-points the current perspective — the title bar and the project
    /// commands that follow it move with the user. An INCIDENTAL activation of
    /// the same file (pane-focus dance) fires only `buffer-switch-hook` and must
    /// leave the perspective put. Same flat, stubbed-persistence harness as
    /// `dropdown_switch_then_first_compile_uses_new_project`.
    #[test]
    fn deliberate_buffer_switch_follows_project_incidental_does_not() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/diagnostics.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        engine.eval(include_str!("../../scheme/helm.scm")).unwrap();
        engine
            .eval(&format!(
                r#"
                (define (persistence-dir) "{}")
                (define *magit-scope-hook* #f)
                (define (set-magit-scope-hook! f) (set! *magit-scope-hook* f))
                "#,
                fresh_persistence_dir()
            ))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/titlebar-menu.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/perspectives.scm"))
            .unwrap();
        let base = std::env::temp_dir().join("macros-persp-follow-test");
        let (proj_a, proj_b) = (base.join("alpha"), base.join("beta"));
        for p in [&proj_a, &proj_b] {
            std::fs::create_dir_all(p).unwrap();
            std::fs::write(p.join(".projectile"), "").unwrap();
            std::fs::write(p.join("main.txt"), "x\n").unwrap();
        }
        let (a, b) = (proj_a.to_str().unwrap(), proj_b.to_str().unwrap());
        let snap = |dir: &str, file: &str| {
            engine.set_snapshot(EditorSnapshot {
                name: "main.txt".to_string(),
                directory: dir.to_string(),
                path: file.to_string(),
                ..Default::default()
            });
        };
        let current =
            |engine: &ScriptEngine| engine.eval("*perspective-current*").unwrap().value.unwrap();
        // Open alpha's file (fresh find-file): perspective becomes alpha.
        snap(a, &format!("{a}/main.txt"));
        engine.call_action("perspective-track", None);
        assert_eq!(current(&engine), format!("\"{a}\""));
        // Now the alpha and beta buffers are both open. Deliberately switch to
        // beta's already-open buffer (C-x b): buffer-select-hook fires and the
        // perspective must follow into beta.
        snap(b, &format!("{b}/main.txt"));
        engine.call_action("perspective-follow", None);
        assert_eq!(
            current(&engine),
            format!("\"{b}\""),
            "deliberate switch to a file in another project must re-point the perspective"
        );
        // An incidental pane-focus activation of alpha's buffer fires only
        // buffer-switch-hook (perspective-note) — the perspective stays beta.
        snap(a, &format!("{a}/main.txt"));
        engine.call_action("perspective-note", None);
        assert_eq!(
            current(&engine),
            format!("\"{b}\""),
            "incidental activation must not re-point the perspective"
        );
        // Switching within the same project must not thrash / re-render need not
        // change the current root.
        snap(b, &format!("{b}/main.txt"));
        engine.call_action("perspective-follow", None);
        assert_eq!(current(&engine), format!("\"{b}\""));
    }

    /// Helix selection-first motions read every selection from the snapshot and
    /// hand a new set back via `set-selections` — the multi-selection core that
    /// scheme/helix.scm drives. Exercised end to end through the engine.
    #[test]
    fn helix_selection_motions_drive_set_selections() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        // "hello world\n" is 12 bytes; the second line starts at byte 12.
        let set = |sels: Vec<(usize, usize)>| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str("hello world\nsecond line\n"),
                selections: sels,
                ..Default::default()
            });
        };
        let sels = |src: &str| match engine.eval(src).unwrap().commands.as_slice() {
            [EditorCommand::SetSelections(p)] => p.clone(),
            other => panic!("expected one SetSelections, got {other:?}"),
        };

        // `l` collapses to a bare cursor one grapheme right.
        set(vec![(0, 0)]);
        assert_eq!(sels("(helix-char-right)"), vec![(1, 1)]);

        // `w` selects from the caret to the next word start: "hello " = (0, 6).
        set(vec![(0, 0)]);
        assert_eq!(sels("(helix-word-next)"), vec![(0, 6)]);

        // On a line's last word it stops before the newline, as Helix does...
        set(vec![(6, 6)]);
        assert_eq!(sels("(helix-word-next)"), vec![(6, 11)]);
        // ...and from there the next `w` skips the break to the next word.
        set(vec![(6, 11)]);
        assert_eq!(sels("(helix-word-next)"), vec![(12, 19)]);
        set(vec![(6, 6)]);
        assert_eq!(sels("(helix-bigword-next)"), vec![(6, 11)]);

        // Two selections move independently — the whole point of multi-selection.
        set(vec![(0, 0), (12, 12)]);
        assert_eq!(sels("(helix-char-right)"), vec![(1, 1), (13, 13)]);

        // Extend mode keeps each anchor and grows only the head.
        set(vec![(0, 0)]);
        engine.eval("(set! *helix-extend* #t)").unwrap();
        assert_eq!(sels("(helix-char-right)"), vec![(0, 1)]);
        engine.eval("(set! *helix-extend* #f)").unwrap();
    }

    /// Helix `gq` (`:reflow` on a key): each selection expands to whole lines,
    /// re-wraps at *fill-column*, and leaves a bare cursor at the block start.
    #[test]
    fn helix_reflow_wraps_selected_lines() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        engine.eval("(set! *fill-column* 4)").unwrap();
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("aaa bbb\n"),
            selections: vec![(0, 0)],
            ..Default::default()
        });
        let cmds = engine.eval("(helix-reflow)").unwrap().commands;
        match cmds.as_slice() {
            [
                EditorCommand::ReplaceRegion {
                    start: 0,
                    end: 7,
                    text,
                },
                EditorCommand::SetSelections(sels),
            ] => {
                assert_eq!(text, "aaa\nbbb");
                assert_eq!(sels, &[(0, 0)]);
            }
            other => panic!("expected ReplaceRegion + SetSelections, got {other:?}"),
        }
    }

    /// The Helix additions: I/A line inserts, select-all, join, case fold, and
    /// selection-first char find — all through the multi-selection plumbing.
    #[test]
    fn helix_line_inserts_join_case_and_find() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let set = |text: &str, sels: Vec<(usize, usize)>| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                selections: sels,
                ..Default::default()
            });
        };

        // I / A — one cursor per line at first non-blank / line end, then insert.
        set("  foo\nbar\n", vec![(3, 3), (8, 8)]);
        let cmds = engine.eval("(helix-insert-line-start)").unwrap().commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetSelections(p) if p == &[(2, 2), (6, 6)])),
            "I should land on each line's first non-blank: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::EvilInsertState)),
            "I should enter insert: {cmds:?}"
        );
        set("  foo\nbar\n", vec![(3, 3), (8, 8)]);
        let cmds = engine.eval("(helix-append-line-end)").unwrap().commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetSelections(p) if p == &[(5, 5), (9, 9)])),
            "A should land on each line end: {cmds:?}"
        );

        // % — the whole buffer as one selection.
        set("hello\n", vec![(0, 0)]);
        let cmds = engine.eval("(helix-select-all)").unwrap().commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetSelections(p) if p == &[(0, 6)])),
            "% should select the whole buffer: {cmds:?}"
        );

        // J — a bare cursor joins the next line up (single-line block extends).
        set("aaa\nbbb\n", vec![(0, 0)]);
        let cmds = engine.eval("(helix-join-lines)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 0, end: 7, text }
                if text == "aaa bbb")
            ),
            "J should join the two lines: {cmds:?}"
        );

        // ~ — toggle case of each selection's text.
        set("aBc\n", vec![(0, 3)]);
        let cmds = engine.eval("(helix-switch-case)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::ReplaceRegion { start: 0, end: 3, text }
                if text == "AbC")
            ),
            "~ should toggle the selection's case: {cmds:?}"
        );

        // f — every head selects to the found char on its own line.
        set("hello world\n", vec![(0, 0)]);
        engine
            .eval("(set! *helix-find-till* #f) (set! *helix-find-forward* #t)")
            .unwrap();
        let cmds = engine
            .eval(r#"(helix-find-dispatch "o")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetSelections(p) if p == &[(0, 5)])),
            "f o should select through the o (Helix includes the target): {cmds:?}"
        );
    }

    /// Regression tests for the Vim/Helix audit: operator ranges, counts
    /// (`(prefix-count)`), EOF linewise deletes, and Helix's cursor-on-last-char
    /// model. Each case checks the commands a Scheme command queues against a
    /// snapshot.
    #[test]
    fn modal_audit_operator_ranges_and_counts() {
        use EditorCommand as C;
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let run = |text: &str, point: usize, count: usize, src: &str| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                point,
                selections: vec![(point, point)],
                prefix_count: count,
                ..Default::default()
            });
            engine.eval(src).unwrap().commands
        };
        let has =
            |cmds: &[EditorCommand], want: &dyn Fn(&EditorCommand) -> bool| cmds.iter().any(want);

        // cw changes the whole word — also from its last char.
        let c = run("hello world", 0, 0, "(evil-change-word)");
        assert!(
            has(&c, &|c| matches!(
                c,
                C::KillRegionBetween { start: 0, end: 5 }
            )),
            "{c:?}"
        );
        let c = run("hello world", 4, 0, "(evil-change-word)");
        assert!(
            has(&c, &|c| matches!(
                c,
                C::KillRegionBetween { start: 4, end: 5 }
            )),
            "{c:?}"
        );

        // dw on a line's last word stops at the line end (no join).
        let c = run("foo bar\n  baz", 4, 0, "(evil-delete-word)");
        assert!(
            has(&c, &|c| matches!(
                c,
                C::KillRegionBetween { start: 4, end: 7 }
            )),
            "{c:?}"
        );

        // dd on a last line with no newline takes the newline before it.
        let c = run("a\nb", 2, 0, "(evil-delete-line)");
        assert!(
            has(&c, &|c| matches!(
                c,
                C::KillRegionBetween { start: 2, end: 3 }
            )),
            "{c:?}"
        );
        assert!(
            has(&c, &|c| matches!(c, C::DeleteRegion { start: 1, end: 2 })),
            "{c:?}"
        );

        // 3dd / 2yy cover several lines in ONE kill.
        let c = run("a\nb\nc\nd\n", 0, 3, "(evil-delete-line)");
        assert!(
            has(&c, &|c| matches!(
                c,
                C::KillRegionBetween { start: 0, end: 6 }
            )),
            "{c:?}"
        );
        let c = run("a\nb\nc", 0, 2, "(evil-yank-line)");
        assert!(
            has(&c, &|c| matches!(c, C::SetRegion { start: 0, end: 4 })),
            "{c:?}"
        );
        assert!(
            has(&c, &|c| matches!(c, C::GotoChar(0))),
            "yy keeps the cursor: {c:?}"
        );

        // 2G — line 2's first non-blank; plain G still means the last line.
        let c = run("a\n  b\nc\n", 0, 2, "(evil-goto-last-line)");
        assert!(has(&c, &|c| matches!(c, C::GotoChar(4))), "{c:?}");
        let c = run("a\n  b\nc\n", 0, 0, "(evil-goto-last-line)");
        assert!(has(&c, &|c| matches!(c, C::GotoChar(6))), "{c:?}");

        // 3x — three chars, clamped to the line.
        let c = run("abcd\n", 2, 3, "(evil-delete-char)");
        assert!(
            has(&c, &|c| matches!(
                c,
                C::KillRegionBetween { start: 2, end: 4 }
            )),
            "{c:?}"
        );

        // J adds no space before `)`; normally one.
        let c = run("foo\n  )bar\n", 0, 0, "(evil-join-line)");
        assert!(
            has(
                &c,
                &|c| matches!(c, C::ReplaceRegion { start: 3, end: 6, text } if text.is_empty())
            ),
            "{c:?}"
        );
        let c = run("foo\nbar\nbaz\n", 0, 3, "(evil-join-line)");
        assert_eq!(
            c.iter()
                .filter(|c| matches!(c, C::ReplaceRegion { text, .. } if text == " "))
                .count(),
            2,
            "3J joins three lines: {c:?}"
        );

        // viw selects the word, cursor ON its last char (Visual is inclusive).
        let c = run("foo bar", 1, 0, r#"(evil-object-cmd "select" "i" "word")"#);
        assert!(
            has(&c, &|c| matches!(c, C::SetRegion { start: 0, end: 2 })),
            "{c:?}"
        );

        // `#` mid-word searches back from the word start.
        let c = run("foo bar foo", 9, 0, "(evil-search-word-backward)");
        assert!(has(&c, &|c| matches!(c, C::GotoChar(8))), "{c:?}");

        // 2f. — the second match.
        run("a.b.c", 0, 2, "(set! *evil-find-count* (evil-count))");
        engine
            .eval("(set! *evil-find-till* #f) (set! *evil-find-forward* #t)")
            .unwrap();
        let c = engine.eval(r#"(evil-find-dispatch ".")"#).unwrap().commands;
        assert!(has(&c, &|c| matches!(c, C::GotoChar(3))), "{c:?}");

        // --- Helix: the cursor is ON a selection's last char -----------------
        let sel = |text: &str, sels: Vec<(usize, usize)>, src: &str| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                selections: sels,
                ..Default::default()
            });
            engine.eval(src).unwrap().commands
        };
        let sels_of = |c: &[EditorCommand]| {
            c.iter()
                .find_map(|c| match c {
                    C::SetSelections(p) => Some(p.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no SetSelections in {c:?}"))
        };
        let text = "hello world\nsecond line\n";
        // e includes the word's last char.
        assert_eq!(
            sels_of(&sel(text, vec![(0, 0)], "(helix-word-end)")),
            vec![(0, 5)]
        );
        // b from a bare cursor mid-word keeps the cursor char.
        assert_eq!(
            sels_of(&sel(text, vec![(13, 13)], "(helix-word-prev)")),
            vec![(14, 12)]
        );
        // gl lands on the last char, not the newline.
        assert_eq!(
            sels_of(&sel(text, vec![(0, 0)], "(helix-goto-line-end)")),
            vec![(10, 10)]
        );
        // a on a bare cursor appends AFTER the char.
        assert_eq!(
            sels_of(&sel(text, vec![(0, 0)], "(helix-append-after)")),
            vec![(1, 1)]
        );
        // x then A appends on the SELECTED line, not the next one.
        assert_eq!(
            sels_of(&sel(text, vec![(0, 12)], "(helix-append-line-end)")),
            vec![(11, 11)]
        );
        // x then o collapses to a bare cursor before opening the line (no delete).
        let c = sel(text, vec![(0, 12)], "(helix-open-below)");
        assert_eq!(sels_of(&c), vec![(11, 11)]);
        assert!(has(&c, &|c| matches!(c, C::Newline)), "{c:?}");
        // x then j moves from the selected line, landing on the next one.
        let j = sels_of(&sel(text, vec![(0, 12)], "(helix-line-down)"));
        assert!(
            (12..=23).contains(&j[0].1),
            "xj should land on line 2: {j:?}"
        );
        // d leaves select mode.
        engine.eval("(set! *helix-extend* #t)").unwrap();
        sel(text, vec![(0, 5)], "(helix-delete)");
        assert_eq!(
            engine.eval("*helix-extend*").unwrap().value.as_deref(),
            Some("#false")
        );
    }

    /// Opening the Helix tutorial switches the editor into Helix mode first, so
    /// the selection-first keys it teaches are live (mirrors the Vim tutorial's
    /// `evil-mode`). The `HelixMode` command must precede the buffer setup.
    #[test]
    fn helix_tutorial_switches_to_helix_mode() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let cmds = engine.eval("(help-with-helix-tutorial)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::HelixMode)),
            "help-with-helix-tutorial must enable Helix mode; got {cmds:?}"
        );
    }

    /// Helix `d` yanks every selection to the clipboard, deletes them
    /// high-offset-first, and leaves one cursor per deletion.
    #[test]
    fn helix_delete_yanks_deletes_and_recollapses() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("hello world\n"),
            selections: vec![(0, 5)], // "hello"
            ..Default::default()
        });
        match engine.eval("(helix-delete)").unwrap().commands.as_slice() {
            [
                EditorCommand::ClipboardSet(text),
                EditorCommand::DeleteRegion { start, end },
                EditorCommand::SetSelections(p),
            ] => {
                assert_eq!(text, "hello");
                assert_eq!((*start, *end), (0, 5));
                assert_eq!(p, &vec![(0, 0)]);
            }
            other => panic!("unexpected command sequence: {other:?}"),
        }
    }

    /// `load-theme-impl` accepts both a symbol and a string, queueing the same
    /// `LoadTheme` command with the bare theme name either way. (The user-facing
    /// `load-theme` wrapper that adds the interactive prompt lives in core.scm.)
    #[test]
    fn load_theme_accepts_symbol_and_string() {
        let (engine, _rx) = ScriptEngine::new();
        for src in [
            r#"(load-theme-impl 'macros-dark)"#,
            r#"(load-theme-impl "macros-dark")"#,
        ] {
            let cmds = engine.eval(src).expect("load-theme should eval").commands;
            match cmds.as_slice() {
                [EditorCommand::LoadTheme(name)] => assert_eq!(name, "macros-dark"),
                other => panic!("{src} should queue LoadTheme(\"macros-dark\"), got {other:?}"),
            }
        }
    }

    /// The argument-aware completion machinery (completion.scm): the form scanner
    /// finds the enclosing head + arg index, the registry resolves a producer, and
    /// `theme-names` feeds the `(load-theme …)` argument completion. Exercises the
    /// pure logic; the live CAPF additionally reads buffer state at completion time.
    #[test]
    fn arg_completion_capf_logic() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let yn = |src: &str| engine.eval(src).unwrap().value;
        // In `(load-theme 'mac…` the context is (head="load-theme", arg-index=0).
        assert_eq!(
            yn(r#"(let ((c (capf-arg-context "(load-theme 'mac")))
                    (if (and c (equal? (car c) "load-theme") (= (cdr c) 0)) "y" "n"))"#)
            .as_deref(),
            Some("\"y\"")
        );
        // Typing the head itself, or being at top level, yields no arg context.
        assert_eq!(
            yn(r#"(if (capf-arg-context "(load-the") "y" "n")"#).as_deref(),
            Some("\"n\"")
        );
        assert_eq!(
            yn(r#"(if (capf-arg-context "mac") "y" "n")"#).as_deref(),
            Some("\"n\"")
        );
        // String args don't desync the counter: arg 2 of (define-key a b c…).
        assert_eq!(
            yn(
                r#"(let ((c (capf-arg-context "(define-key \"mode\" \"chord\" \"comm")))
                    (if (and c (equal? (car c) "define-key") (= (cdr c) 2)) "y" "n"))"#
            )
            .as_deref(),
            Some("\"y\"")
        );
        // Symbol prefix keeps hyphens but drops the leading quote.
        assert_eq!(
            yn(r#"(if (equal? (capf-symbol-prefix "(load-theme 'macros-d") "macros-d") "y" "n")"#)
                .as_deref(),
            Some("\"y\"")
        );
        // Comments with stray parens/quotes/apostrophes don't desync the counter:
        // the `(set-face)` and unbalanced `(` and the apostrophe live in comments.
        assert_eq!(
            yn("(let ((c (capf-arg-context \"\
                 ;; it's a (set-face thing with an unmatched (\n\
                 (load-theme 'mac\")))\
                 (if (and c (equal? (car c) \"load-theme\") (= (cdr c) 0)) \"y\" \"n\"))")
            .as_deref(),
            Some("\"y\"")
        );
        // The registry wires load-theme arg 0 to a theme-name producer.
        assert_eq!(
            yn(r#"(if (procedure? (arg-completion-lookup "load-theme" 0)) "y" "n")"#).as_deref(),
            Some("\"y\"")
        );
        assert_eq!(
            yn(
                r#"(if (member "macros-dark" ((arg-completion-lookup "load-theme" 0) "")) "y" "n")"#
            )
            .as_deref(),
            Some("\"y\"")
        );
    }

    /// `M-x load-theme` (no args) opens a "Load theme:" completing-read whose
    /// candidates are the bundled theme names, with itself as the callback.
    #[test]
    fn load_theme_no_args_prompts_with_theme_candidates() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        match engine.eval("(load-theme)").unwrap().commands.as_slice() {
            [
                EditorCommand::CompletingRead {
                    prompt,
                    candidates,
                    callback,
                    ..
                },
            ] => {
                assert!(prompt.contains("theme"), "prompt was {prompt:?}");
                assert_eq!(callback, "load-theme");
                assert!(candidates.iter().any(|c| c == "macros-dark"));
                assert!(candidates.iter().any(|c| c == "macros-light"));
            }
            other => panic!("expected a CompletingRead over themes, got {other:?}"),
        }
    }

    /// End-to-end CAPF: with a snapshot whose buffer is `(load-theme 'ma` and
    /// point at end, `arg-completion-capf` reads `(point)`/`(buffer-substring)`
    /// and returns the theme names — the path the editor's auto-complete drives.
    #[test]
    fn arg_completion_capf_returns_themes_from_snapshot() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        let text = "(load-theme 'ma";
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str(text),
            point: text.len(),
            ..Default::default()
        });
        // The CAPF ignores its prefix arg and computes from buffer state.
        let v = engine
            .eval(r#"(arg-completion-capf "ma")"#)
            .unwrap()
            .value
            .unwrap_or_default();
        assert!(
            v.contains("macros-dark") && v.contains("macros-light"),
            "CAPF should return theme names, got: {v}"
        );
    }

    /// language-modes.scm generates an `<id>-mode` command per bundled grammar:
    /// each is M-x discoverable AND callable (the `eval-string` define persists),
    /// and invoking it switches the buffer's major mode. Scheme is excluded (it
    /// has the richer steel-mode / macros-steel-mode commands).
    #[test]
    fn language_modes_are_generated_for_every_grammar() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        for lang in crate::highlight::BUNDLED_LANGUAGES {
            let cmd = format!("{lang}-mode");
            if *lang == "scheme" {
                // Excluded: no generated scheme-mode (steel-mode covers it).
                assert_eq!(
                    engine
                        .eval(&format!(
                            r#"(if (member "{cmd}" *interactive-commands*) "y" "n")"#
                        ))
                        .unwrap()
                        .value
                        .as_deref(),
                    Some("\"n\""),
                    "scheme-mode should not be generated"
                );
                continue;
            }
            // Registered for M-x.
            assert_eq!(
                engine
                    .eval(&format!(
                        r#"(if (member "{cmd}" *interactive-commands*) "y" "n")"#
                    ))
                    .unwrap()
                    .value
                    .as_deref(),
                Some("\"y\""),
                "{cmd} is not registered for M-x"
            );
            // Callable, and switches the major mode to the language id.
            let commands = engine.eval(&format!("({cmd})")).unwrap().commands;
            assert!(
                commands
                    .iter()
                    .any(|c| matches!(c, EditorCommand::SetMajorMode(m) if m == lang)),
                "({cmd}) did not enqueue SetMajorMode({lang}): {commands:?}"
            );
        }
    }

    /// `package-install`'s URL→name derivation and quoted-symbol option parsing.
    /// The doc index powers discoverability: built-ins carry a source location,
    /// `index-doc-file` folds a freshly loaded package's documented defines in,
    /// and `helm-apropos`'s live filter matches on doc text — not just names.
    #[test]
    fn doc_index_locates_builtins_and_indexes_packages() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script failed to load");
        }
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();

        // A built-in resolves to its source file + a 1-based line.
        assert_eq!(v(r#"(null? (function-source "save-excursion"))"#), "#false");
        assert_eq!(
            v(r#"(ends-with? (car (function-source "save-excursion")) ".scm")"#),
            "#true"
        );
        assert_eq!(
            v(r#"(> (cadr (function-source "save-excursion")) 0)"#),
            "#true"
        );
        // An unknown name has no location.
        assert_eq!(v(r#"(null? (function-source "no-such-fn-xyz"))"#), "#true");

        // A package file on disk: index it, then its commented define is both
        // documented and locatable — exactly what pkg--load does at load time.
        let dir = std::env::temp_dir().join(format!("macros-docidx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pkg.scm");
        std::fs::write(
            &file,
            ";; Set the editor font size to N points.\n(define (pkg-font-size n) n)\n",
        )
        .unwrap();
        let path = file.to_string_lossy().replace('"', "\\\"");

        // Before indexing: unknown.
        assert_eq!(v(r#"(function-doc "pkg-font-size")"#), "\"\"");

        engine
            .eval(&format!(r#"(index-doc-file "{path}")"#))
            .unwrap();

        // After indexing: doc + source location both present.
        assert_eq!(
            v(r#"(function-doc "pkg-font-size")"#),
            "\"Set the editor font size to N points.\""
        );
        assert_eq!(
            v(&format!(
                r#"(equal? (car (function-source "pkg-font-size")) "{path}")"#
            )),
            "#true"
        );

        // The live picker filters on doc text: searching "font size" surfaces the
        // package fn even though its NAME doesn't contain that phrase. Candidates
        // are `(name summary)` pairs, so map to the bare name before checking.
        assert_eq!(
            v(r#"(if (member "pkg-font-size"
                            (map (lambda (c) (if (list? c) (car c) c))
                                 (helm-apropos-candidates "font size")))
                     #t #f)"#),
            "#true"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Matching is multi-word (order-independent), tolerates a plural query
    /// word, and options are in the index — so the natural "font size" query
    /// finds the `font-size` option even though it is not a `(define …)`.
    #[test]
    fn helm_apropos_matching_is_multiword_and_covers_options() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script failed to load");
        }
        // #t iff `name` is among the candidate names for `query`.
        let finds = |query: &str, name: &str| {
            engine
                .eval(&format!(
                    r#"(if (member "{name}"
                                  (map (lambda (c) (if (list? c) (car c) c))
                                       (helm-apropos-candidates "{query}")))
                          #t #f)"#
                ))
                .unwrap()
                .value
                .unwrap()
        };

        // Options are searchable, and the natural query resolves to the key.
        assert_eq!(
            engine
                .eval(r#"(if (member "font-size" (option-list)) #t #f)"#)
                .unwrap()
                .value
                .unwrap(),
            "#true"
        );
        assert_eq!(finds("font size", "font-size"), "#true");
        // Word order doesn't matter.
        assert_eq!(finds("size font", "font-size"), "#true");
        // A plural query word still finds the singular in the key.
        assert_eq!(finds("font sizes", "font-size"), "#true");
        // A word that appears in neither name nor doc excludes the candidate.
        assert_eq!(finds("font orange", "font-size"), "#false");
    }

    #[test]
    fn package_url_and_option_parsing() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script failed to load");
        }
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();
        // The leaf segment, with any trailing slash and ".git" stripped.
        assert_eq!(
            v(r#"(pkg--name-from-url "https://github.com/u/macros-foo.git")"#),
            "\"macros-foo\""
        );
        assert_eq!(
            v(r#"(pkg--name-from-url "https://github.com/u/macros-foo")"#),
            "\"macros-foo\""
        );
        assert_eq!(
            v(r#"(pkg--name-from-url "https://github.com/u/macros-foo/")"#),
            "\"macros-foo\""
        );
        // Option lookup over the quoted-symbol plist.
        assert_eq!(
            v(r#"(pkg--opt (list 'ref "v1" 'entry "x.scm") 'entry)"#),
            "\"x.scm\""
        );
        // A missing option is #f (test via a branch — bool repr aside).
        assert_eq!(
            v(r#"(if (pkg--opt (list 'ref "v1") 'entry) "y" "n")"#),
            "\"n\""
        );
    }

    /// End-to-end: clone a local git repo with `pkg--git`, then `pkg--load` its
    /// entry — the entry's definitions land in the engine and the package is
    /// marked loaded (so `'depends` can see it). Exercises the git shell-out and
    /// the error-isolated load path. Skips cleanly if `git` isn't on PATH.
    #[test]
    fn package_clone_and_load() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no git in this environment
        }
        let root = std::env::temp_dir().join(format!("macros-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("macros-demo.scm"), "(define demo-loaded 42)\n").unwrap();
        // A repo with one commit (so the clone carries the entry file). Pass the
        // identity inline so it works even where git has no global user config.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&src)
                .output()
                .unwrap()
        };
        git(&["init", "--quiet"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "."]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "x",
        ]);

        let dest = root.join("packages").join("macros-demo");
        let (engine, _rx) = ScriptEngine::new();
        for s in crate::runtime::startup_scripts() {
            engine.eval(s).unwrap();
        }

        // Clone, then confirm the working tree arrived.
        let clone = format!(
            r#"(pkg--git (list "clone" "--quiet" "{}" "{}") "clone")"#,
            src.display(),
            dest.display()
        );
        engine.eval(&clone).unwrap();
        assert!(
            dest.join("macros-demo.scm").exists(),
            "clone did not produce the entry file"
        );

        // Load the entry; its define becomes a live binding and it's marked loaded.
        let load = format!(
            r#"(pkg--load "macros-demo" "{}" "macros-demo.scm" #f)"#,
            dest.display()
        );
        engine.eval(&load).unwrap();
        assert_eq!(
            engine.eval("demo-loaded").unwrap().value.as_deref(),
            Some("42")
        );
        assert_eq!(
            engine
                .eval(r#"(if (package-loaded? "macros-demo") "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"y\"")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `include` helper (over Steel's `load`) evaluates another file in the
    /// same engine, so its `define-key` lands as a real command.
    #[test]
    fn include_loads_relative_file() {
        let dir = std::env::temp_dir().join(format!("macros-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("keys.scm"),
            r#"(define-key "fundamental" "ctrl-z" "undo")"#,
        )
        .unwrap();

        let (engine, _rx) = ScriptEngine::new();
        // The same prelude main.rs injects before init.scm.
        engine.eval(&crate::runtime::include_prelude(&dir)).unwrap();
        let result = engine.eval(r#"(include "keys.scm")"#).unwrap();
        assert!(
            matches!(
                result.commands.as_slice(),
                [EditorCommand::DefineKey(mode, chord, cmd)]
                    if mode == "fundamental" && chord == "ctrl-z" && cmd == "undo"
            ),
            "include should run keys.scm's define-key, got {:?}",
            result.commands
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `call-process` runs a subprocess on the engine thread and returns stdout.
    #[test]
    fn call_process_returns_stdout() {
        let (engine, _rx) = ScriptEngine::new();
        // Compare via a Steel-side trim — the printed repr escapes the trailing
        // newline, so assert on content, not the debug display.
        let out = engine
            .eval(r#"(trim-end (call-process "echo" (list "hello" "world")))"#)
            .unwrap();
        assert_eq!(out.value.as_deref(), Some("\"hello world\""));
    }

    /// `setenv` records into the spawn overlay and `getenv` reads it back — the
    /// bridge config uses to extend PATH for subprocess spawns. Children see
    /// it; the real process env (unsound to mutate from a thread) does not.
    #[test]
    fn setenv_then_getenv_roundtrips() {
        let (engine, _rx) = ScriptEngine::new();
        let out = engine
            .eval(r#"(begin (setenv "MACROS_TEST_VAR" "abc/123") (getenv "MACROS_TEST_VAR"))"#)
            .unwrap();
        assert_eq!(out.value.as_deref(), Some("\"abc/123\""));
        assert!(std::env::var("MACROS_TEST_VAR").is_err());
        let out = engine
            .eval(r#"(trim-end (call-process "/bin/sh" (list "-c" "printf %s \"$MACROS_TEST_VAR\"")))"#)
            .unwrap();
        assert_eq!(out.value.as_deref(), Some("\"abc/123\""));
    }

    /// A lambda where a callback *name* is expected is an error that says how
    /// to pass one, instead of being silently dropped (output went nowhere).
    #[test]
    fn lambda_callbacks_are_rejected_with_a_hint() {
        let (engine, _rx) = ScriptEngine::new();
        let Err(err) = engine.eval(r#"(make-process "true" '() (lambda (s) s) "")"#) else {
            panic!("a lambda callback should be refused");
        };
        assert!(err.contains("callback->name"), "{err}");
        // Names and "no callback" still work.
        assert!(
            engine
                .eval(r#"(make-process "true" '() 'ignore #f)"#)
                .is_ok()
        );
    }

    /// Negative integers for unsigned parameters are refused instead of
    /// wrapping to 2^64 (a line indent of 1.8e19 columns, a timer that never
    /// fires) — both for helper-registered commands and direct closures.
    #[test]
    fn negative_ints_for_unsigned_params_are_errors() {
        let (engine, _rx) = ScriptEngine::new();
        for src in [
            "(set-line-indent 0 -1)",
            "(run-with-timer -1 'f)",
            "(line-indentation -1)",
            "(marker-position -1)",
        ] {
            let Err(err) = engine.eval(src) else {
                panic!("{src} should fail");
            };
            assert!(err.contains("non-negative"), "{src}: {err}");
        }
        // Valid calls still work, and reals accept integers.
        assert!(engine.eval("(set-line-indent 0 2)").is_ok());
        assert!(engine.eval("(split-window-right-ratio 1)").is_ok());
        assert!(engine.eval("(set-line-scale 0 2)").is_ok());
    }

    /// A dynamic Helm source that errors reports the error instead of showing
    /// an empty list with no explanation.
    #[test]
    fn failing_candidate_source_reports_its_error() {
        use futures::StreamExt;
        let (engine, mut rx) = ScriptEngine::new();
        engine.dispatch_candidates(0, 1, "no-such-source-fn", "x");
        let mut saw_error = false;
        for _ in 0..2 {
            match futures::executor::block_on(rx.next()).unwrap() {
                ScriptOutcome::Commands {
                    message: Some(m), ..
                } => {
                    saw_error = m.contains("no-such-source-fn");
                }
                ScriptOutcome::Candidates { candidates, .. } => assert!(candidates.is_empty()),
                _ => {}
            }
        }
        assert!(saw_error);
    }

    /// A blocking call with a deadline interrupts a runaway function instead
    /// of hanging (the UI thread waits on these), and the engine stays usable.
    #[test]
    fn call_action_within_interrupts_a_runaway_loop() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval("(define (spin) (spin)) (define (quick) (message \"hi\"))")
            .unwrap();
        let started = std::time::Instant::now();
        let (_, error) = engine.call_action_within("spin", std::time::Duration::from_millis(200));
        assert!(error.is_some_and(|e| e.contains("timed out")));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let (commands, error) =
            engine.call_action_within("quick", std::time::Duration::from_secs(5));
        assert_eq!(error, None);
        assert!(matches!(commands.as_slice(), [EditorCommand::Message(_)]));
    }

    /// C-g interrupts a runaway async eval (a looping `M-:`), but leaves a job
    /// that only just started alone; the engine keeps working afterwards.
    #[test]
    fn interrupt_running_stops_a_runaway_async_eval() {
        use std::time::{Duration, Instant};
        let (engine, _rx) = ScriptEngine::new();
        engine.eval("(define (spin) (spin))").unwrap();
        engine.dispatch("(spin)", true);
        // Wait until the loop has run past the C-g age floor.
        std::thread::sleep(Duration::from_millis(400));
        assert!(engine.interrupt_running(), "C-g should interrupt the loop");
        let started = Instant::now();
        assert_eq!(
            engine.eval("(+ 1 2)").unwrap().value.as_deref(),
            Some("3"),
            "the engine survives the interrupt"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // Nothing running: C-g has nothing to interrupt.
        assert!(!engine.interrupt_running());
    }

    /// A blocking call made while an async job is stuck reports the engine busy
    /// at once instead of freezing the UI for its whole timeout.
    #[test]
    fn call_action_within_fails_fast_behind_a_stuck_job() {
        use std::time::{Duration, Instant};
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval("(define (spin) (spin)) (define (quick) (message \"hi\"))")
            .unwrap();
        engine.dispatch("(spin)", true);
        std::thread::sleep(Duration::from_millis(2500));
        let started = Instant::now();
        let (commands, error) = engine.call_action_within("quick", Duration::from_secs(10));
        assert!(commands.is_empty());
        assert!(error.is_some_and(|e| e.contains("busy")));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(engine.interrupt_running());
    }

    /// `(render-buffer name mode text faces)` enqueues a `RenderBuffer` with the
    /// `(line face)` pairs parsed into `line_faces`.
    #[test]
    fn render_buffer_parses_faces() {
        let (engine, _rx) = ScriptEngine::new();
        let result = engine
            .eval(
                r#"(render-buffer "*magit*" "magit-status" "a\nb"
                       (list (list 0 "magit-section-heading") (list 1 "magit-file")))"#,
            )
            .unwrap();
        match result.commands.as_slice() {
            [
                EditorCommand::RenderBuffer {
                    name,
                    mode,
                    text,
                    line_faces,
                    quiet: false,
                },
            ] => {
                assert_eq!(name, "*magit*");
                assert_eq!(mode, "magit-status");
                assert_eq!(text, "a\nb");
                assert_eq!(
                    line_faces,
                    &[
                        (0, "magit-section-heading".to_string()),
                        (1, "magit-file".to_string())
                    ]
                );
            }
            other => panic!("expected one RenderBuffer, got {other:?}"),
        }
    }

    /// Overlay / fold / text-property primitives parse to their commands.
    #[test]
    fn overlay_primitives_parse() {
        let (engine, _rx) = ScriptEngine::new();
        match engine
            .eval(r#"(overlay-set-face 3 8 "diff-added")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::OverlayFace { start, end, face }] => {
                assert_eq!((*start, *end, face.as_str()), (3, 8, "diff-added"));
            }
            other => panic!("expected OverlayFace, got {other:?}"),
        }
        // The batch form queues one OverlayFace per (start end face) triple,
        // skipping malformed entries.
        match engine
            .eval(
                r#"(overlay-set-faces (list (list 3 8 "diff-added")
                                            (list "bad")
                                            (list 9 12 "diff-removed")))"#,
            )
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::OverlayFace {
                    start: 3,
                    end: 8,
                    face: f1,
                },
                EditorCommand::OverlayFace {
                    start: 9,
                    end: 12,
                    face: f2,
                },
            ] => {
                assert_eq!((f1.as_str(), f2.as_str()), ("diff-added", "diff-removed"));
            }
            other => panic!("expected two OverlayFace, got {other:?}"),
        }
        assert!(matches!(
            engine.eval("(fold-lines 2 5)").unwrap().commands.as_slice(),
            [EditorCommand::FoldLines {
                start_line: 2,
                end_line: 5
            }]
        ));
        assert!(matches!(
            engine.eval("(unfold-all)").unwrap().commands.as_slice(),
            [EditorCommand::UnfoldAll]
        ));
        assert!(matches!(
            engine
                .eval("(set-fold-marker 3 #t)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::SetFoldMarker {
                line: 3,
                open: true
            }]
        ));
        assert!(matches!(
            engine
                .eval("(clear-fold-markers)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ClearFoldMarkers]
        ));
        match engine
            .eval(r#"(put-text-property 0 4 "section" "staged")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::PutTextProperty {
                    start,
                    end,
                    key,
                    value,
                },
            ] => {
                assert_eq!(
                    (*start, *end, key.as_str(), value.as_str()),
                    (0, 4, "section", "staged")
                );
            }
            other => panic!("expected PutTextProperty, got {other:?}"),
        }
    }

    /// Keymap introspection (help/discoverability) + the M-x execute mechanism.
    #[test]
    fn keymap_introspection_and_mx() {
        let (engine, _rx) = ScriptEngine::new();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // command-list exposes the built-ins.
        assert_eq!(
            v(r#"(if (member "forward-char" (command-list)) "yes" "no")"#),
            "\"yes\""
        );
        // define-key populates the shadow keymap → key-binding reads it back.
        engine
            .eval(r#"(define-key "fundamental" "ctrl-z" "undo")"#)
            .unwrap();
        assert_eq!(v(r#"(key-binding "fundamental" "ctrl-z")"#), "\"undo\"");
        assert_eq!(
            v(r#"(key-binding "fundamental" "ctrl-q")"#),
            "\"\"",
            "unbound = empty"
        );
        // A later definition wins (matches the real keymap).
        engine
            .eval(r#"(define-key "fundamental" "ctrl-z" "redo")"#)
            .unwrap();
        assert_eq!(v(r#"(key-binding "fundamental" "ctrl-z")"#), "\"redo\"");
        // eval-string is what M-x's execute-command runs.
        assert_eq!(v(r#"(eval-string "(+ 1 2)")"#), "3");
    }

    /// The editing lib's pure helpers.
    #[test]
    fn editing_lib_helpers() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/editing.scm"))
            .unwrap();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        assert_eq!(v(r#"(car (sort-strings (list "c" "a" "b")))"#), "\"a\"");
        assert_eq!(v(r#"(capitalize-token "hELLO")"#), "\"Hello\"");
        assert_eq!(
            v(r#"(capitalize-words "foo\nbAR\tbaz qux")"#),
            "\"Foo\\nBar\\tBaz Qux\""
        );
        assert_eq!(v(r#"(string-join (list "a" "b" "c") "-")"#), "\"a-b-c\"");
        // sort-lines registered itself for M-x.
        assert_eq!(
            v(r#"(if (member "sort-lines" *interactive-commands*) "yes" "no")"#),
            "\"yes\""
        );
    }

    /// Byte-offset splitting stays correct across multibyte text: string-match
    /// returns BYTE offsets, so the "split at match" loops must take the head
    /// with the byte-safe `byte-take` (a char-indexed substring corrupts the
    /// text on any non-ASCII char before the match). Covers the shared core
    /// helper plus the org consumers: TBLFM substitution, planning-line
    /// repeater shifts, and noweb expansion.
    #[test]
    fn org_byte_safe_split_at_match() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // org.scm and its full declared dep set (core, folding, bookmarks -
        // which pulls helm/persistence/fundamental), in startup order.
        for src in crate::runtime::script_with_deps("org") {
            engine.eval(src).expect("org.scm and its deps");
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // The core helper itself: é is 2 bytes, so the first 5 bytes of
        // "café!" are exactly "café"; over-length takes the whole string.
        assert_eq!(v(r#"(byte-take "café!" 5)"#), "\"café\"");
        assert_eq!(v(r#"(byte-take "abc" 99)"#), "\"abc\"");
        assert_eq!(v(r#"(byte-take "abc" 0)"#), "\"\"");

        // TBLFM field substitution with non-ASCII before the reference: the
        // prefix must survive byte-for-byte ("é→" is 5 bytes but 2 chars).
        assert_eq!(
            v(r#"(org--subst-dollar (list (list "2" "3")) 1 "é→$1*$2")"#),
            "\"é→(2)*(3)\""
        );
        assert_eq!(
            v(r#"(org--subst-at (list (list "5")) "π=@1$1")"#),
            "\"π=(5)\""
        );

        // Repeater shift on a planning line with multibyte text before the
        // stamp: the prefix is preserved and the stamp advances.
        assert_eq!(
            v(r#"(org--shift-planning "  按 SCHEDULED: <2026-06-25 Thu +1d>")"#),
            "\"  按 SCHEDULED: <2026-06-26 Fri +1d>\""
        );

        // Noweb expansion in a src block with a multibyte char before the
        // <<ref>>: the named block's body replaces the ref, prefix intact.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("#+name: blk\n#+begin_src sh\ninner\n#+end_src\n"),
            ..Default::default()
        });
        assert_eq!(
            v(r#"(org--expand-noweb "café <<blk>> done")"#),
            "\"café inner done\""
        );
    }

    /// The Org library loads (top-level set-face/define-key/hooks all resolve)
    /// and its pure parser / exporter / table helpers behave.
    #[test]
    fn org_lib_pure_functions() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // org.scm registers its chevron handler with the folding registry.
        // org.scm and its full declared dep set (core, folding, bookmarks -
        // which pulls helm/persistence/fundamental), in startup order.
        for src in crate::runtime::script_with_deps("org") {
            engine.eval(src).expect("org.scm and its deps");
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // Heading detection + TODO cycle.
        assert_eq!(v(r#"(org--heading-level "** TODO x")"#), "2");
        assert_eq!(v(r#"(if (org--heading-level "plain line") 1 0)"#), "0");
        // C-c C-t rotates the full cycle: nothing → first keyword → … → "".
        assert_eq!(
            v(r#"(org--next-keyword #f (org--cycle-keywords))"#),
            "\"TODO\""
        );
        assert_eq!(
            v(r#"(org--next-keyword "TODO" (org--cycle-keywords))"#),
            "\"NEXT\""
        );
        assert_eq!(
            v(r#"(org--next-keyword "CANCELLED" (org--cycle-keywords))"#),
            "\"\""
        ); // last → clear
        // Custom keywords are user-defined: the built-in ships none, so a word is
        // recognized only after the user adds it to *org-custom-keywords*.
        assert_eq!(v(r#"(length *org-custom-keywords*)"#), "0");
        assert_eq!(v(r#"(if (org--custom-keyword? "BUG") 1 0)"#), "0");
        engine
            .eval(r#"(set! *org-custom-keywords* (list "BUG" "FEATURE"))"#)
            .unwrap();
        // Now recognized for fontification/parsing AND folded into the C-c C-t
        // cycle, after the TODO vocabulary: …→CANCELLED→BUG→FEATURE→"".
        assert_eq!(v(r#"(if (org--custom-keyword? "BUG") 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--custom-keyword? "TODO") 1 0)"#), "0");
        assert_eq!(
            v(r#"(org--leading-keyword "BUG crash on save")"#),
            "\"BUG\""
        );
        assert_eq!(
            v(r#"(nth (car (org-parse "* FEATURE dark mode")) 2)"#),
            "\"FEATURE\""
        );
        assert_eq!(
            v(r#"(org--next-keyword "CANCELLED" (org--cycle-keywords))"#),
            "\"BUG\""
        );
        assert_eq!(
            v(r#"(org--next-keyword "BUG" (org--cycle-keywords))"#),
            "\"FEATURE\""
        );
        assert_eq!(
            v(r#"(org--next-keyword "FEATURE" (org--cycle-keywords))"#),
            "\"\""
        ); // last → clear
        // A custom keyword has no color (inherits the title) until the user sets
        // one, then re-registers the faces.
        assert_eq!(v(r#"(if (org--keyword-color "BUG") 1 0)"#), "0");
        assert_eq!(v(r#"(org--keyword-face-name "BUG")"#), "\"org-kw-BUG\"");
        engine
            .eval(r##"(set! *org-custom-keyword-colors* (list (cons "BUG" "#ff0000")))"##)
            .unwrap();
        engine
            .eval(r#"(org--register-custom-keyword-faces)"#)
            .unwrap();
        assert_eq!(v(r#"(org--keyword-color "BUG")"#), "\"#ff0000\"");
        assert_eq!(v(r#"(if (org--keyword-color "FEATURE") 1 0)"#), "0"); // still uncolored

        // Parser: a heading + a paragraph; heading fields are extracted.
        assert_eq!(v(r#"(length (org-parse "* A\nbody text\n"))"#), "2");
        assert_eq!(
            v(r#"(car (car (org-parse "* TODO Buy milk :home:")))"#),
            "\"heading\""
        );
        assert_eq!(
            v(r#"(nth (car (org-parse "* TODO Buy milk :home:")) 2)"#),
            "\"TODO\""
        );
        assert_eq!(
            v(r#"(nth (car (org-parse "* TODO Buy milk :home:")) 4)"#),
            "\"Buy milk\""
        );
        assert_eq!(
            v(r#"(nth (car (org-parse "* TODO Buy milk :home:")) 5)"#),
            "\"home\""
        );

        // Export smoke: a heading becomes an <h1>.
        assert_eq!(
            v(r#"(if (string-contains? (org--ast->html (org-parse "* Hi")) "<h1>") 1 0)"#),
            "1"
        );

        // Table align pads each column to its widest cell.
        assert_eq!(
            v(r#"(car (org--format-table (list "| a | bb |" "| ccc | d |")))"#),
            "\"| a   | bb |\""
        );
        // Padding is by CHARS, not bytes: "é" is one display column (2 bytes),
        // so it pads to the ASCII neighbour's width instead of falling a
        // column short and skewing the pipes.
        assert_eq!(
            v(r#"(org--format-table (list "| é | b |" "| cc | d |"))"#),
            "(\"| é  | b |\" \"| cc | d |\")"
        );
        // Separator dashes track the same char widths as the data rows.
        assert_eq!(
            v(r#"(org--format-table (list "| é |" "|-|"))"#),
            "(\"| é |\" \"|---|\")"
        );

        // RET-in-table escape hatch (org.el's org-return): a plain newline at
        // end of line or in the leading indentation, table-next-row otherwise.
        assert_eq!(v(r#"(if (org--return-escapes? "| a | b |" 9) 1 0)"#), "1"); // EOL
        assert_eq!(v(r#"(if (org--return-escapes? "| a | b |" 0) 1 0)"#), "1"); // BOL
        assert_eq!(v(r#"(if (org--return-escapes? "  | a |" 1) 1 0)"#), "1"); // indent
        assert_eq!(v(r#"(if (org--return-escapes? "| a | b |" 4) 1 0)"#), "0"); // mid-cell

        // Inline-image link detection: only image-typed, undescribed links.
        assert_eq!(
            v(r#"(if (org--image-link-path "[[file:diagram.png]]") 1 0)"#),
            "1"
        );
        assert_eq!(
            v(r#"(if (org--image-link-path "[[notes.org][see]]") 1 0)"#),
            "0"
        );
        assert_eq!(v(r#"(if (org--image-link-path "plain text") 1 0)"#), "0");
        // Absolute paths pass through; relative ones resolve against the buffer dir.
        assert_eq!(
            v(r#"(org--image-link-path "[[/tmp/a.PNG]]")"#),
            "\"/tmp/a.PNG\""
        );

        // LaTeX fragment extraction: inner math of the first $…$.
        assert_eq!(
            v(r#"(org--latex-fragment "before $x^2 + 1$ after")"#),
            "\"x^2 + 1\""
        );
        assert_eq!(v(r#"(if (org--latex-fragment "no math here") 1 0)"#), "0");

        // Interactive-table helpers. Pipe byte positions and cell-at-column.
        assert_eq!(v(r#"(org--pipe-positions "| a | bb |")"#), "(0 4 9)");
        assert_eq!(v(r#"(org--cell-index-at "| a | bb |" 2)"#), "0"); // in 'a'
        assert_eq!(v(r#"(org--cell-index-at "| a | bb |" 6)"#), "1"); // in 'bb'
        assert_eq!(v(r#"(org--cell-index-at "| a | bb |" 0)"#), "0"); // before 1st pipe
        // Column count and padding to a fixed width.
        assert_eq!(v(r#"(org--table-ncols (list "| a | b |" "| c |"))"#), "2");
        assert_eq!(v(r#"(org--cells-pad "| x |" 3)"#), "(\"x\" \"\" \"\")");
        // List swap and row rebuild from cells.
        assert_eq!(
            v(r#"(org--list-swap (list "a" "b" "c") 0 2)"#),
            "(\"c\" \"b\" \"a\")"
        );
        assert_eq!(
            v(r#"(org--row-from-cells (list "a" "bb"))"#),
            "\"| a | bb |\""
        );
        // A column swap reformats and aligns: cols of "a"/"bb" become "bb"/"a".
        assert_eq!(
            v(r#"(car (org--format-table
                        (list (org--row-from-cells
                                (org--list-swap (org--cells-pad "| a | bb |" 2) 0 1)))))"#),
            "\"| bb | a |\"",
        );
        // Empty-row generator: n pipes parses back to n-1 empty cells.
        assert_eq!(v(r"(length (org--table-cells (org--pipes 4)))"), "3");
        // Byte prefix sums lengths + newlines.
        assert_eq!(v(r#"(org--rows-byte-prefix (list "ab" "cde") 1)"#), "3");

        // Three-state fold ranges. Outline: 0:* A 1:body 2:** B 3:body 4:** C 5:* D
        engine
            .eval(r"(define HS (list (list 0 1) (list 2 2) (list 4 2) (list 5 1)))")
            .unwrap();
        // A's subtree is lines 1..4; its direct children are the level-2 B and C.
        assert_eq!(v(r"(org--min-child-level HS 0 4)"), "2");
        assert_eq!(v(r"(org--direct-children HS 0 4)"), "(2 4)");
        // FOLDED hides the whole subtree (1..4).
        assert_eq!(
            v(r"(car (org--fold-ranges-for HS 6 0 (quote folded)))"),
            "(1 . 4)"
        );
        // CHILDREN hides A's own body (1..1) and each child's body; child C (line 4)
        // has none, so that degenerate range is dropped by the reapply guard.
        assert_eq!(
            v(r"(length (org--fold-ranges-for HS 6 0 (quote children)))"),
            "3"
        );
        assert_eq!(
            v(r"(car (org--fold-ranges-for HS 6 0 (quote children)))"),
            "(1 . 1)"
        );
        // A leaf heading (no body) folds to nothing.
        assert_eq!(
            v(r"(length (org--fold-ranges-for HS 6 5 (quote folded)))"),
            "0"
        );

        // Local TAB cycle. A heading with children walks subtree→folded→children→…
        assert_eq!(v(r"(org--cycle-next (quote subtree) #t)"), "folded");
        assert_eq!(v(r"(org--cycle-next (quote folded) #t)"), "children");
        assert_eq!(v(r"(org--cycle-next (quote children) #t)"), "subtree");
        // A childless heading skips the no-op 'children view: folded→subtree, so a
        // collapsed leaf expands in a single TAB (the reported two-TAB bug).
        assert_eq!(v(r"(org--cycle-next (quote folded) #f)"), "subtree");
        assert_eq!(v(r"(org--cycle-next (quote subtree) #f)"), "folded");

        // --- Drawer folding -------------------------------------------------
        // `:end:` is matched case-insensitively and distinct from an opener.
        assert_eq!(v(r#"(if (org--drawer-end-line? ":END:") 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--drawer-opener? ":tools:") 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--drawer-opener? ":end:") 1 0)"#), "0");
        // Pairing opener→`:end:` over a line list (lines 1..3 are the drawer).
        engine
            .eval(r#"(define DL (list "intro" ":tools:" "  a" "  b" ":end:" "after"))"#)
            .unwrap();
        assert_eq!(v(r"(length (org--pair-drawers DL))"), "1");
        assert_eq!(v(r"(car (org--pair-drawers DL))"), "(1 . 4)");
        // An unterminated opener (still streaming) is dropped — no fold.
        assert_eq!(
            v(r#"(length (org--pair-drawers (list ":tools:" "  a")))"#),
            "0"
        );

        // --- Layer 7: dates -------------------------------------------------
        assert_eq!(v(r"(ymd->days 1970 1 1)"), "0");
        assert_eq!(v(r"(days->ymd 0)"), "(1970 1 1)");
        assert_eq!(v(r#"(org--dow-name 0)"#), "\"Thu\"");
        assert_eq!(
            v(r#"(org--days->date-str (ymd->days 2026 6 25))"#),
            "\"2026-06-25 Thu\""
        );
        assert_eq!(v(r#"(org--read-date "+1w" 10)"#), "17");
        assert_eq!(v(r#"(org--read-date "today" 42)"#), "42");
        assert_eq!(v(r#"(org--apply-offset "+2d" 0)"#), "2");
        // Repeater shift on a "+" stamp advances from the stamp's own date.
        assert_eq!(
            v(r#"(org--shift-stamp "<2026-06-25 Thu +1d>")"#),
            "\"<2026-06-26 Fri +1d>\""
        );
        // The time of day and a warning period survive the shift.
        assert_eq!(
            v(r#"(org--shift-stamp "<2026-06-25 Thu 10:00 +1w -2d>")"#),
            "\"<2026-07-02 Thu 10:00 +1w -2d>\""
        );

        // --- Layer 7b: property drawers -------------------------------------
        assert_eq!(
            v(
                r#"(cdr (org--assoc-ci (org--drawer-props (list ":PROPERTIES:" ":ID: abc" ":END:") 0 2) "id"))"#
            ),
            "\"abc\""
        );
        assert_eq!(
            v(r#"(car (org--render-drawer (list (cons "ID" "x"))))"#),
            "\":PROPERTIES:\""
        );

        // --- Layer 7d: clock duration --------------------------------------
        assert_eq!(v(r#"(org--fmt-duration 90)"#), "\"1:30\"");
        assert_eq!(v(r#"(org--stamp-minutes "[1970-01-01 Thu 01:30]")"#), "90");

        // --- Layer 7f: capture templates -----------------------------------
        assert_eq!(
            v(r#"(org--expand-template-final "* TODO %?" "buy milk")"#),
            "\"* TODO buy milk\""
        );

        // --- Layer 7g: tag matching ----------------------------------------
        assert_eq!(v(r#"(if (org--tag-match "a+b" (list "a" "b")) 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--tag-match "a+b" (list "a")) 1 0)"#), "0");
        assert_eq!(v(r#"(if (org--tag-match "a|c" (list "c")) 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--tag-match "-x" (list "a")) 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--tag-match "-x" (list "x")) 1 0)"#), "0");

        // --- Layer 7h/6: markup export -------------------------------------
        assert_eq!(
            v(r#"(if (string-contains? (org--inline->html "+x+") "<del>") 1 0)"#),
            "1"
        );

        // --- Layer 7i: table formula evaluation ----------------------------
        assert_eq!(v(r#"(org--eval-arith "1+2*3")"#), "7");
        assert_eq!(v(r#"(org--eval-arith "(1+2)*3")"#), "9");
        assert_eq!(v(r#"(org--fmt-num (org--eval-arith "10/4"))"#), "\"2.5\"");
        assert_eq!(v(r#"(org--fmt-num (org--eval-arith "2+3"))"#), "\"5\"");
        assert_eq!(v(r#"(org--fmt-num (org--eval-arith "-3+1"))"#), "\"-2\"");
        assert_eq!(
            v(r#"(org--row-set (list "1" "2") 3 "9")"#),
            "(\"1\" \"2\" \"9\")"
        );
        assert_eq!(
            v(r#"(nth (car (org--apply-formula (list (list "1" "2" "")) "$3=$1+$2")) 2)"#),
            "\"3\""
        );

        // --- Layer 7j: export backends -------------------------------------
        assert_eq!(
            v(r##"(if (string-contains? (org--ast->md (org-parse "* Hi")) "# Hi") 1 0)"##),
            "1"
        );
        assert_eq!(v(r#"(org--inline->md "*x*")"#), "\"**x**\"");
        assert_eq!(
            v(
                r#"(if (string-contains? (org--ast->latex (org-parse "* Hi")) "\\section{Hi}") 1 0)"#
            ),
            "1"
        );

        // --- Layer 7k/l: columns + list-item prefix ------------------------
        assert_eq!(
            v(r#"(cdr (car (org--parse-columns "%40ITEM %TODO")))"#),
            "\"ITEM\""
        );
        assert_eq!(
            v(r#"(car (car (org--parse-columns "%40ITEM %TODO")))"#),
            "40"
        );
        assert_eq!(v(r#"(if (org--list-line-prefix "- item") 1 0)"#), "1");
        assert_eq!(v(r#"(if (org--list-line-prefix "plain") 1 0)"#), "0");
        // `* Foo` at column 0 is a heading; an indented `*` is a bullet.
        assert_eq!(v(r#"(if (org--list-line-prefix "* Foo") 1 0)"#), "0");
        assert_eq!(v(r#"(if (org--list-line-prefix "  * item") 1 0)"#), "1");
    }

    /// org-roam.scm loads on top of core + org and its new pure helpers behave.
    /// (Its references to org.scm functions are all in call position, so loading
    /// it after org.scm resolves them.)
    #[test]
    fn org_roam_extended_helpers() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // org.scm and its full declared dep set (core, folding, bookmarks -
        // which pulls helm/persistence/fundamental), in startup order.
        for src in crate::runtime::script_with_deps("org") {
            engine.eval(src).expect("org.scm and its deps");
        }
        engine
            .eval(include_str!("../../scheme/org-roam.scm"))
            .expect("org-roam.scm");
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // Slug + tag splitting.
        assert_eq!(v(r#"(org-roam--slug "Hello, World!")"#), "\"hello-world\"");
        assert_eq!(v(r#"(org-roam--split-tags ":a:b:")"#), "(\"a\" \"b\")");
        // Alias/ref value parsing: quoted tokens, else whitespace split.
        assert_eq!(
            v(r#"(org-roam--parse-quoted-list "\"alpha one\" \"beta\"")"#),
            "(\"alpha one\" \"beta\")"
        );
        assert_eq!(
            v(r#"(org-roam--parse-quoted-list "alpha beta")"#),
            "(\"alpha\" \"beta\")"
        );
        // file-meta reads :KEY: front matter and #+KEY: keywords.
        assert_eq!(
            v(r#"(org-roam--file-meta ":ROAM_REFS: https://x.test" "ROAM_REFS")"#),
            "\"https://x.test\""
        );
        // Template expansion fills ${title}/${slug}/${id}.
        assert_eq!(
            v(r#"(org-roam--expand-tmpl "${slug}.org" "My Note" "my-note" "ID1")"#),
            "\"my-note.org\""
        );
        // Daily date arithmetic (self-contained calendar).
        assert_eq!(
            v(r#"(org-roam--date-str (+ 1 (org-roam--date-str->days "2026-06-25")))"#),
            "\"2026-06-26\""
        );
        // Link-description rewrite for rename.
        assert_eq!(
            v(r#"(org-roam--rewrite-links "see [[id:ID1][Old]] end" "ID1" "New")"#),
            "\"see [[id:ID1][New]] end\""
        );
    }

    /// folding.scm's indentation model: which lines are fold headers and how far
    /// each block extends. The snapshot-backed line readers are shadowed with a
    /// pure fixture so the algorithm is exercised without a live buffer.
    ///
    /// Fixture (indent per line, -1 = blank):
    ///   0 `def foo():`   header → body lines 1..2
    ///   1     `a`        4
    ///   2     `b`        4
    ///   3     (blank)   -1
    ///   4 `def bar():`   header → body line 5
    ///   5     `c`        4
    ///   6 `x`            0  not a header (nothing deeper follows)
    #[test]
    fn folding_indentation_model() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // Define the line readers as pure Scheme over a fixture *before* loading
        // folding.scm, so its references bind to these rather than the native
        // snapshot bridges (a live buffer would otherwise be needed).
        engine
            .eval(
                r"(define *fix* (list 0 4 4 -1 0 4 0))
                  (define (line-count) (length *fix*))
                  (define (line-indentation i) (nth *fix* i))",
            )
            .expect("fixture");
        engine
            .eval(include_str!("../../scheme/folding.scm"))
            .expect("folding.scm");
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // Header detection.
        assert_eq!(v("(if (fold--header-indent 0 7) 1 0)"), "1");
        assert_eq!(v("(if (fold--header-indent 4 7) 1 0)"), "1");
        assert_eq!(v("(if (fold--header-indent 1 7) 1 0)"), "0"); // peer line
        assert_eq!(v("(if (fold--header-indent 3 7) 1 0)"), "0"); // blank
        assert_eq!(v("(if (fold--header-indent 6 7) 1 0)"), "0"); // nothing under
        // Block extent (inclusive last body line; trailing blank not absorbed).
        assert_eq!(v("(fold--block-end 0 0 7)"), "2");
        assert_eq!(v("(fold--block-end 4 0 7)"), "5");

        // A toggle emits the right chevron states: folding line 0 hides its body
        // (FoldLines 1..3) and flips marker 0 to closed (open=false), while the
        // untouched header 4 stays open. Toggling back reopens it.
        let fold = engine.eval("(fold-toggle-at-line 0)").unwrap().commands;
        assert!(
            fold.iter().any(|c| matches!(
                c,
                EditorCommand::SetFoldMarker {
                    line: 0,
                    open: false
                }
            )),
            "fold should close marker 0: {fold:?}"
        );
        assert!(fold.iter().any(|c| matches!(
            c,
            EditorCommand::SetFoldMarker {
                line: 4,
                open: true
            }
        )));
        assert!(fold.iter().any(|c| matches!(
            c,
            EditorCommand::FoldLines {
                start_line: 1,
                end_line: 3
            }
        )));
        let unfold = engine.eval("(fold-toggle-at-line 0)").unwrap().commands;
        assert!(
            unfold.iter().any(|c| matches!(
                c,
                EditorCommand::SetFoldMarker {
                    line: 0,
                    open: true
                }
            )),
            "unfold should reopen marker 0: {unfold:?}"
        );
        assert!(
            !unfold
                .iter()
                .any(|c| matches!(c, EditorCommand::FoldLines { .. })),
            "unfold should not re-hide anything: {unfold:?}"
        );
    }

    /// `set-line-image`'s optional width arg: omitted → 0 (default cap); given →
    /// passed through. Exercises the core.scm wrapper over the `-impl` primitive.
    #[test]
    fn set_line_image_optional_width() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        assert!(matches!(
            engine.eval(r#"(set-line-image 1 "/x.png")"#).unwrap().commands.as_slice(),
            [EditorCommand::SetLineImage { line: 1, path, max_width: 0 }] if path == "/x.png"
        ));
        assert!(matches!(
            engine
                .eval(r#"(set-line-image 1 "/x.png" 800)"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::SetLineImage {
                line: 1,
                max_width: 800,
                ..
            }]
        ));
    }

    /// The virtual-indent primitives enqueue the right commands.
    #[test]
    fn set_line_indent_primitive() {
        let (engine, _rx) = ScriptEngine::new();
        assert!(matches!(
            engine
                .eval(r"(set-line-indent 3 4)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::SetLineIndent { line: 3, cols: 4 }]
        ));
        assert!(matches!(
            engine
                .eval(r"(clear-line-indents)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ClearLineIndents]
        ));
    }

    /// `guard-buffer-unchanged` captures the snapshot's buffer name and edit
    /// tick at call time — the editor drops the rest of the batch when either
    /// no longer matches at application time (a stale debounced repaint).
    #[test]
    fn guard_buffer_unchanged_primitive() {
        let (engine, _rx) = ScriptEngine::new();
        engine.set_snapshot(EditorSnapshot {
            name: "notes.org".into(),
            tick: 7,
            ..Default::default()
        });
        assert!(matches!(
            engine
                .eval(r#"(guard-buffer-unchanged "org--fontify-schedule")"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::GuardBufferUnchanged { name, tick: 7, on_stale }]
                if name == "notes.org" && on_stale == "org--fontify-schedule"
        ));
    }

    /// `org--fontify` opens its batch with a `GuardBufferUnchanged` captured
    /// BEFORE any text is read, so a repaint raced by live typing is dropped
    /// whole instead of painting faces/hides/indents at stale byte offsets
    /// (the "headings below the edit blink while typing" bug). The indent pass
    /// shares the guard: `set-line-indent` rows follow in the same batch.
    #[test]
    fn org_fontify_batch_is_guarded() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // org.scm and its full declared dep set (core, folding, bookmarks -
        // which pulls helm/persistence/fundamental), in startup order.
        for src in crate::runtime::script_with_deps("org") {
            engine.eval(src).expect("org.scm and its deps");
        }
        engine.set_snapshot(EditorSnapshot {
            name: "notes.org".into(),
            tick: 3,
            text: Rope::from_str("* top\nbody\n** child\n"),
            ..Default::default()
        });
        let commands = engine.eval("(org--fontify)").unwrap().commands;
        assert!(
            matches!(
                commands.first(),
                Some(EditorCommand::GuardBufferUnchanged { name, tick: 3, on_stale })
                    if name == "notes.org" && on_stale == "org--fontify-schedule"
            ),
            "the guard must be the batch's FIRST command: {commands:?}"
        );
        // Body line 1 sits one level under the level-1 heading (1 * 4 columns);
        // the level-2 heading on line 2 is indented (2-1) * 4 columns.
        for expect in [(1usize, 4usize), (2, 4)] {
            assert!(
                commands.iter().any(|c| matches!(
                    c,
                    EditorCommand::SetLineIndent { line, cols } if (*line, *cols) == expect
                )),
                "missing indent {expect:?}: {commands:?}"
            );
        }
        // Faces accumulate in *org--spans* and flush through the batched
        // overlay-set-faces call — the heading faces must reach the batch.
        for face in ["org-level-1", "org-level-2"] {
            assert!(
                commands.iter().any(|c| matches!(
                    c,
                    EditorCommand::OverlayFace { face: f, .. } if f == face
                )),
                "missing {face} overlay: {commands:?}"
            );
        }
    }

    /// The per-line scale primitives enqueue the right commands.
    #[test]
    fn set_line_scale_primitive() {
        let (engine, _rx) = ScriptEngine::new();
        assert!(matches!(
            engine
                .eval(r"(set-line-scale 2 1.6)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::SetLineScale { line: 2, scale }] if (*scale - 1.6).abs() < 1e-4
        ));
        assert!(matches!(
            engine
                .eval(r"(clear-line-scales)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ClearLineScales]
        ));
    }

    /// The ghost-text primitives enqueue the right commands.
    #[test]
    fn ghost_text_primitives() {
        let (engine, _rx) = ScriptEngine::new();
        assert!(matches!(
            engine
                .eval(r#"(ghost-text-show "let x = 1;")"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::GhostShow { text }] if text == "let x = 1;"
        ));
        assert!(matches!(
            engine
                .eval(r"(ghost-text-clear)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::GhostClear]
        ));
        assert!(matches!(
            engine
                .eval(r"(ghost-text-accept)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::GhostAccept]
        ));
    }

    /// The markdown library loads and its heading detection (which drives the
    /// marker-hiding and heading-scale fontify pass) is correct.
    #[test]
    fn markdown_lib_heading_helpers() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        // markdown.scm registers its chevron handler with the folding registry.
        engine
            .eval(include_str!("../../scheme/folding.scm"))
            .expect("folding.scm");
        engine
            .eval(include_str!("../../scheme/markdown.scm"))
            .expect("markdown.scm");
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // ATX heading levels; a `#` with no following space is not a heading.
        assert_eq!(v("(md-heading-level \"## Title\")"), "2");
        assert_eq!(v("(md-heading-level \"#### deep\")"), "4");
        assert_eq!(v("(md-heading-level \"# Top\")"), "1");
        assert_eq!(v("(md-heading-level \"not a heading\")"), "0");
        assert_eq!(v("(md-heading-level \"#nospace\")"), "0");
        // `#` lines inside fenced code are masked (same length), not headings.
        assert_eq!(
            v("(md--mask-fences (list \"```sh\" \"# c\" \"```\" \"# H\"))"),
            "(\"```sh\" \"  c\" \"```\" \"# H\")"
        );

        // Scale tapers from h1 down, and every heading is larger than body text.
        assert_eq!(
            v("(> (md--heading-scale 1) (md--heading-scale 2))"),
            "#true"
        );
        assert_eq!(v("(> (md--heading-scale 4) 1)"), "#true");

        // The structure signature (which gates re-fontify, fixing the per-key
        // blink) is stable when only title text changes, but differs when a
        // heading's level changes — so editing a title skips the repaint.
        assert_eq!(
            v("(equal? (md--heading-signature (list \"# A\" \"body\")) \
                       (md--heading-signature (list \"# Z\" \"text\")))"),
            "#true"
        );
        assert_eq!(
            v("(equal? (md--heading-signature (list \"# A\")) \
                       (md--heading-signature (list \"## A\")))"),
            "#false"
        );
        // Inserting a (non-heading) line — pressing Enter — leaves the signature
        // unchanged, so no repaint fires and the headers don't blink.
        assert_eq!(
            v("(equal? (md--heading-signature (list \"# A\" \"body\")) \
                       (md--heading-signature (list \"# A\" \"\" \"body\")))"),
            "#true"
        );

        // Section extent + folding. Lines: 0 "# A", 1 body, 2 body, 3 "# B", 4 body.
        // A's body is the half-open [1, 3); folding it must hide BOTH body lines
        // (this guards the off-by-one that used to leave the last body line shown).
        engine
            .eval("(define L (list \"# A\" \"b1\" \"b2\" \"# B\" \"b3\"))")
            .unwrap();
        assert_eq!(v("(md--section-body-end L 0)"), "3");
        assert_eq!(v("(md--section-body-end L 3)"), "5"); // last section runs to EOF
        // Foldable headings are those with a body (both here have one).
        assert_eq!(v("(length (md--foldable-headings L))"), "2");
        assert_eq!(v("(car (md--foldable-headings L))"), "0");
        // Folding section A must hide its whole body [1,3) — not [1,2).
        engine.eval("(md--folded-set (list 0))").unwrap();
        let cmds = engine.eval("(md--reapply-folds L)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::FoldLines {
                    start_line: 1,
                    end_line: 3
                }
            )),
            "section A should fold [1,3): {cmds:?}"
        );
    }

    /// The org-roam library loads and its pure helpers + the new id/time
    /// primitives behave.
    #[test]
    fn org_roam_lib_pure_functions() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/org-roam.scm"))
            .expect("org-roam.scm");
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // The two new Rust primitives.
        assert_eq!(v(r"(string-length (org-id-new))"), "36"); // v4 UUID
        assert_eq!(v(r#"(string-length (current-time "%Y%m%d%H%M%S"))"#), "14");
        assert_eq!(v(r#"(string-length (current-time "%Y-%m-%d"))"#), "10");

        // Slugify: lowercase, collapse non-alphanumerics, trim.
        assert_eq!(v(r#"(org-roam--slug "Hello, World!")"#), "\"hello-world\"");
        assert_eq!(v(r#"(org-roam--slug "  ??? ")"#), "\"untitled\"");

        // filetags parsing and grep-hit parsing.
        assert_eq!(
            v(r#"(string-join (org-roam--split-tags ":work:urgent:") "|")"#),
            "\"work|urgent\""
        );
        assert_eq!(
            v(r#"(nth (org-roam--parse-hit "/n/a.org:12:see [[id:x]]") 1)"#),
            "12"
        );
        assert_eq!(
            v(r#"(nth (org-roam--parse-hit "/n/a.org:12:see [[id:x]]") 0)"#),
            "\"/n/a.org\""
        );

        // Node lookups over a hand-built index (5-element records: …line).
        engine
            .eval(
                r#"(set! *org-roam-nodes* (list (list "ID1" "Alpha" "/n/a.org" '() 0)
                                                  (list "ID2" "Beta" "/n/b.org" '() 7)))"#,
            )
            .unwrap();
        assert_eq!(v(r#"(nth (org-roam--node-by-title "Beta") 0)"#), "\"ID2\"");
        assert_eq!(v(r#"(nth (org-roam--node-by-id "ID1") 1)"#), "\"Alpha\"");
        assert_eq!(
            v(r#"(string-join (org-roam--titles) "|")"#),
            "\"Alpha|Beta\""
        );
        // Subtree node carries its headline line; file-level node is line 0.
        assert_eq!(
            v(r#"(org-roam--node-line (org-roam--node-by-id "ID2"))"#),
            "7"
        );

        // Subtree parsing: headline title cleaned, drawer :ID: found below it.
        assert_eq!(
            v(r#"(org-roam--heading-text "** TODO My Topic :tag:")"#),
            "\"TODO My Topic\""
        );
        assert_eq!(
            v(r#"(org-roam--drawer-id (list ":PROPERTIES:" ":ID:  abc-123" ":END:"))"#),
            "\"abc-123\""
        );
        assert_eq!(
            v(r#"(if (org-roam--drawer-id (list "plain text")) 1 0)"#),
            "0"
        );

        // Link-id extraction (drives graph edges).
        assert_eq!(
            v(r#"(string-join (org-roam--link-ids "see [[id:AA][x]] and [[id:BB]]") "|")"#),
            "\"AA|BB\""
        );
        // DOT label sanitising: embedded quotes become apostrophes.
        assert_eq!(v(r#"(org-roam--dot-escape "a\"b")"#), "\"a'b\"");
    }

    /// Rename porcelain entries ("OLD -> NEW") split into real pathspecs so git
    /// add/reset/checkout don't get the literal arrow string as one path.
    #[test]
    fn magit_rename_pathspecs() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();
        // Rename: both sides, NEW is the on-disk path.
        assert_eq!(
            v(r#"(string-join (magit-pathspecs "a/old.svg -> a/new.svg") "|")"#),
            "\"a/old.svg|a/new.svg\""
        );
        assert_eq!(
            v(r#"(magit-newpath "a/old.svg -> a/new.svg")"#),
            "\"a/new.svg\""
        );
        // Plain path: a single pathspec, unchanged.
        assert_eq!(
            v(r#"(string-join (magit-pathspecs "a/plain.txt") "|")"#),
            "\"a/plain.txt\""
        );
        assert_eq!(v(r#"(magit-newpath "a/plain.txt")"#), "\"a/plain.txt\"");
    }

    /// C-g (magit-keyboard-quit) kills a live async push/pull via the lib-async
    /// "magit" lane; with nothing running it falls back to plain keyboard-quit.
    #[test]
    fn magit_keyboard_quit_cancels_async_git() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");

        // Idle — no live git subprocess: the ordinary quit.
        assert!(matches!(
            engine
                .eval("(magit-keyboard-quit)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::KeyboardQuit]
        ));

        // A push is in flight (seed the lane as magit-run-async does): C-g
        // kills that process instead of quitting, and flags the run as
        // cancelled so the exit callback won't report failure.
        engine
            .eval(r#"(async-lane-run "magit" "/bin/sh" (list) 'magit-async-output 'magit-async-exit)"#)
            .unwrap();
        assert!(matches!(
            engine
                .eval("(magit-keyboard-quit)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::KillProcess { .. }]
        ));
        assert_eq!(
            engine
                .eval(r#"(if (async-lane-cancelled? "magit") "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"y\"")
        );
        // The exit callback's async-lane-finish reports the cancel and frees
        // the lane for the next push.
        assert_eq!(
            engine
                .eval(r#"(if (async-lane-finish "magit") "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"y\"")
        );
        assert_eq!(
            engine
                .eval(r#"(if (async-lane-live? "magit") "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"n\"")
        );
    }

    /// The lib-async lane lifecycle: run spawns and marks the lane live, a
    /// second run is refused while live, cancel kills by id, finish frees.
    #[test]
    fn async_lane_lifecycle() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();

        // Run: enqueues MakeProcess, lane goes live.
        assert!(matches!(
            engine
                .eval(r#"(async-lane-run "t" "/bin/sh" (list) 'out 'exit)"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::MakeProcess { .. }]
        ));
        assert_eq!(v(r#"(if (async-lane-live? "t") "y" "n")"#), "\"y\"");

        // A second run on a live lane is refused: a message, no MakeProcess.
        assert!(matches!(
            engine
                .eval(r#"(async-lane-run "t" "/bin/sh" (list) 'out 'exit)"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::Message(_)]
        ));

        // Cancel kills the recorded process id and marks the lane cancelled.
        assert!(matches!(
            engine
                .eval(r#"(async-lane-cancel "t")"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::KillProcess { .. }]
        ));
        assert_eq!(v(r#"(if (async-lane-cancelled? "t") "y" "n")"#), "\"y\"");

        // Finish (the exit callback) reports the cancel and frees the lane.
        assert_eq!(v(r#"(if (async-lane-finish "t") "y" "n")"#), "\"y\"");
        assert_eq!(v(r#"(if (async-lane-live? "t") "y" "n")"#), "\"n\"");
        // A finish after a natural (uncancelled) exit reports #f.
        engine
            .eval(r#"(async-lane-run "t" "/bin/sh" (list) 'out 'exit)"#)
            .unwrap();
        assert_eq!(v(r#"(if (async-lane-finish "t") "y" "n")"#), "\"n\"");

        // Idle lane: cancel is a no-op #f, and the quit helper falls back to
        // the ordinary keyboard-quit.
        assert_eq!(v(r#"(if (async-lane-cancel "t") "y" "n")"#), "\"n\"");
        assert!(matches!(
            engine
                .eval(r#"(async-lane-quit "t")"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::KeyboardQuit]
        ));
    }

    /// The evil overlay binds vim motions for magit WITHOUT removing magit's
    /// own single-key commands from the base keymap. `g g` / `g r` coexist with
    /// the base `g` = refresh: under evil the dispatch prefers the overlay
    /// prefix (so `g g` reaches the top), and with evil OFF the base `g` still
    /// refreshes, untouched. `0` / `^` are exposed because magit leaves them free
    /// (`$` stays `magit-process-buffer`, matching evil-collection).
    #[test]
    fn magit_evil_overlay_motions() {
        let (engine, _rx) = ScriptEngine::new();
        let cmds = load_with_deps(&engine, "magit").commands;
        let bound = |mode: &str, key: &str, cmd: &str| {
            cmds.iter().any(|c| {
                matches!(c, EditorCommand::DefineKey(m, s, x)
                if m == mode && s == key && x == cmd)
            })
        };
        // Overlay (evil-only) motions: top/bottom, relocated refresh, line-start.
        assert!(bound(
            "evil-normal:magit-status",
            "g g",
            "evil-goto-first-line"
        ));
        assert!(bound(
            "evil-normal:magit-status",
            "shift-g",
            "evil-goto-last-line"
        ));
        assert!(bound("evil-normal:magit-status", "g r", "magit-refresh"));
        assert!(bound(
            "evil-normal:magit-status",
            "0",
            "move-beginning-of-line"
        ));
        assert!(bound(
            "evil-normal:magit-status",
            "^",
            "evil-first-non-blank"
        ));
        // Base keymap unchanged: single `g` still refreshes (the evil-off path),
        // and `$` is magit's process buffer, never an end-of-line motion.
        assert!(bound("magit-status", "g", "magit-refresh"));
        assert!(bound("magit-status", "$", "magit-process-buffer"));
    }

    /// The `## …` branch header from `git status --branch` parses into
    /// (branch upstream ahead behind) — replacing 3 separate git spawns.
    #[test]
    fn magit_branch_line_parse() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();
        let pipe = |line: &str| format!(r#"(string-join (magit-parse-branch-line "{line}") "|")"#);
        // No upstream.
        assert_eq!(v(&pipe("## main")), "\"main||0|0\"");
        // Upstream, in sync.
        assert_eq!(
            v(&pipe("## main...origin/main")),
            "\"main|origin/main|0|0\""
        );
        // Both ahead and behind.
        assert_eq!(
            v(&pipe("## main...origin/main [ahead 1, behind 2]")),
            "\"main|origin/main|1|2\""
        );
        // One-sided.
        assert_eq!(
            v(&pipe("## main...origin/main [ahead 3]")),
            "\"main|origin/main|3|0\""
        );
        assert_eq!(
            v(&pipe("## main...origin/main [behind 4]")),
            "\"main|origin/main|0|4\""
        );
        // Fresh repo and detached HEAD.
        assert_eq!(v(&pipe("## No commits yet on main")), "\"main||0|0\"");
        assert_eq!(v(&pipe("## HEAD (no branch)")), "\"HEAD (detached)||0|0\"");
    }

    /// The expanded magit command set builds its transient menus.
    #[test]
    fn magit_transients_build() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        // Build transients as if invoked from inside a magit buffer, so the
        // repo-association guard (which refuses fileless buffers like *scratch*)
        // short-circuits and we exercise the transient structure itself.
        engine.set_snapshot(EditorSnapshot {
            name: "*magit*".into(),
            ..Default::default()
        });

        match engine
            .eval("(magit-branch-popup)")
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::Transient { title, columns, .. }] => {
                assert_eq!(title, "Branch");
                let rows: Vec<_> = columns.iter().flat_map(|c| &c.rows).collect();
                // checkout, checkout-remote, create, set-upstream, rename, delete
                assert_eq!(rows.len(), 6);
            }
            other => panic!("expected a Transient, got {other:?}"),
        }
        // The dispatch menu lists the command groups; Push uses the raw `shift-p`
        // chord (prettified to "P" only at display time).
        match engine.eval("(magit-dispatch)").unwrap().commands.as_slice() {
            [EditorCommand::Transient { title, columns, .. }] => {
                assert_eq!(title, "Magit");
                let rows: Vec<_> = columns.iter().flat_map(|c| &c.rows).collect();
                assert!(rows.iter().any(|r| r.key == "shift-p" && r.label == "Push"));
                assert!(rows.iter().any(|r| r.command == "magit-stash-popup"));
            }
            other => panic!("expected a Transient, got {other:?}"),
        }
    }

    /// magit diff line classification + sub-line accent byte offsets.
    #[test]
    fn magit_diff_rendering() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");

        // Face classification.
        for (line, face) in [
            ("@@ -1 +1 @@", "magit-hunk"),
            ("+added", "magit-added"),
            ("-removed", "magit-removed"),
            (" context", "magit-context"),
        ] {
            assert_eq!(
                engine
                    .eval(&format!("(magit-diff-face {line:?})"))
                    .unwrap()
                    .value
                    .as_deref(),
                Some(&*format!("\"{face}\"")),
            );
        }

        // Accent byte offsets: "Head!" is 5 bytes + newline = 6, so the next
        // line's "+" marker accent is byte [6, 7).
        engine.eval("(section-begin)").unwrap();
        engine
            .eval(r#"(magit-emit "Head!" "magit-section-heading")"#)
            .unwrap();
        engine.eval(r#"(magit-emit-diff-line "+x")"#).unwrap();
        assert_eq!(
            engine
                .eval("(nth (car *section-accents*) 0)")
                .unwrap()
                .value
                .as_deref(),
            Some("6")
        );
        assert_eq!(
            engine
                .eval("(nth (car *section-accents*) 1)")
                .unwrap()
                .value
                .as_deref(),
            Some("7")
        );
        assert_eq!(
            engine
                .eval("(nth (car *section-accents*) 2)")
                .unwrap()
                .value
                .as_deref(),
            Some("\"magit-add-marker\"")
        );

        // utf8-length counts BYTES, not chars (é = 2 bytes) — accent offsets stay
        // correct after multibyte lines.
        assert_eq!(
            engine
                .eval(r#"(utf8-length "café")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("5")
        );
    }

    /// Inline-blame: `git blame --line-porcelain` parses into per-line records,
    /// the age helpers humanize/tint by author-time, and uncommitted lines are
    /// recognized.
    #[test]
    fn magit_blame_porcelain() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();

        // Two commits over three lines (lines 1-2 share a commit; line 3 differs),
        // built as the list of porcelain lines the parser consumes. Each block ends
        // with the tab-prefixed content line.
        engine
            .eval(
                r#"(define blame-sample (list
                     "abc1234500000000000000000000000000000000 1 1 2"
                     "author Alice"
                     "author-time 100"
                     "summary First commit subject"
                     "filename f.txt"
                     "\tline one"
                     "abc1234500000000000000000000000000000000 2 2"
                     "author Alice"
                     "author-time 100"
                     "summary First commit subject"
                     "filename f.txt"
                     "\tline two"
                     "def4567800000000000000000000000000000000 3 3 1"
                     "author Bob"
                     "author-time 200"
                     "summary Second commit"
                     "filename f.txt"
                     "\tline three"))
                   (define blame-recs (magit-blame-parse blame-sample))"#,
            )
            .unwrap();

        // One record per source line, in file order.
        assert_eq!(v("(length blame-recs)"), "3");
        // Record 1: line 1, Alice's commit, summary carried through.
        assert_eq!(v("(magit-blame-rec-line (list-ref blame-recs 0))"), "1");
        assert_eq!(v("(list-ref (list-ref blame-recs 0) 2)"), "\"Alice\"");
        assert_eq!(
            v("(list-ref (list-ref blame-recs 0) 4)"),
            "\"First commit subject\""
        );
        // Lines 1-2 share a SHA; line 3 is a different commit (so the collapse in
        // `magit-blame-annotate` will annotate lines 1 and 3, not line 2).
        assert_eq!(
            v(
                "(equal? (magit-blame-rec-sha (list-ref blame-recs 0)) (magit-blame-rec-sha (list-ref blame-recs 1)))"
            ),
            "#true"
        );
        assert_eq!(
            v(
                "(equal? (magit-blame-rec-sha (list-ref blame-recs 1)) (magit-blame-rec-sha (list-ref blame-recs 2)))"
            ),
            "#false"
        );

        // Header detection: a 40-hex SHA header is recognized; a key line is not.
        assert_eq!(
            v(r#"(magit-blame-hex40? "abc1234500000000000000000000000000000000 1 1 2")"#),
            "#true"
        );
        assert_eq!(v(r#"(magit-blame-hex40? "author Alice")"#), "#false");

        // Ancient epochs (1970) humanize to "years ago" and tint old.
        assert_eq!(
            v(r#"(ends-with? (magit-blame-humanize 100) "years ago")"#),
            "#true"
        );
        assert_eq!(v("(magit-blame-age-face 100)"), "\"magit-blame-old\"");

        // Uncommitted (all-zero SHA) is recognized and rendered plainly.
        assert_eq!(
            v(
                r#"(magit-blame-uncommitted? (list 1 "0000000000000000000000000000000000000000" "Not Committed Yet" 0 ""))"#
            ),
            "#true"
        );
        assert_eq!(
            v(
                r#"(magit-blame-rec-suffix (list 1 "0000000000000000000000000000000000000000" "Not Committed Yet" 0 ""))"#
            ),
            "\"  Not committed yet\""
        );
        // A committed record's suffix carries short-hash, author, and summary.
        assert_eq!(
            v(r#"(string-contains? (magit-blame-rec-suffix (list-ref blame-recs 0)) "abc12345")"#),
            "#true"
        );
        assert_eq!(
            v(
                r#"(string-contains? (magit-blame-rec-suffix (list-ref blame-recs 0)) "First commit subject")"#
            ),
            "#true"
        );
    }

    /// Range-editing + undo-boundary + buffer-local commands parse.
    #[test]
    fn editing_primitives_parse() {
        let (engine, _rx) = ScriptEngine::new();
        assert!(matches!(
            engine
                .eval("(delete-region 2 5)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::DeleteRegion { start: 2, end: 5 }]
        ));
        assert!(matches!(
            engine
                .eval("(kill-region-between 2 5)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::KillRegionBetween { start: 2, end: 5 }]
        ));
        assert!(matches!(
            engine.eval(r#"(replace-region 1 4 "hi")"#).unwrap().commands.as_slice(),
            [EditorCommand::ReplaceRegion { start: 1, end: 4, text }] if text == "hi"
        ));
        assert!(matches!(
            engine.eval(r#"(insert-at 7 "x")"#).unwrap().commands.as_slice(),
            [EditorCommand::InsertAt { pos: 7, text }] if text == "x"
        ));
        assert!(matches!(
            engine.eval("(undo-boundary)").unwrap().commands.as_slice(),
            [EditorCommand::UndoBoundary]
        ));
        assert!(matches!(
            engine.eval(r#"(buffer-local-set "k" "v")"#).unwrap().commands.as_slice(),
            [EditorCommand::BufferLocalSet { key, value }] if key == "k" && value == "v"
        ));
    }

    /// Regexp primitives (pure string ops).
    #[test]
    fn regexp_primitives() {
        let (engine, _rx) = ScriptEngine::new();
        let v = |s: &str| engine.eval(s).unwrap().value.unwrap();
        assert_eq!(v(r#"(string-match "b+" "aabbb")"#), "2");
        assert_eq!(v(r#"(string-match "z" "abc")"#), "-1");
        assert_eq!(v(r#"(length (regexp-captures "(a)(b)" "ab"))"#), "3");
        assert_eq!(v(r#"(car (regexp-captures "(a)(b)" "xaby"))"#), "\"ab\"");
        assert_eq!(
            v(r#"(replace-regexp-in-string "a" "X" "banana")"#),
            "\"bXnXnX\""
        );
        // regexp-quote escapes regex metacharacters (".": 1 char → 2).
        assert_eq!(v(r#"(string-length (regexp-quote "a.b"))"#), "4");
    }

    /// Markers track positions across edits via `adjust_markers`.
    #[test]
    fn markers_track_edits() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval("(define m (make-marker 10))").unwrap();
        assert_eq!(
            engine.eval("(marker-position m)").unwrap().value.as_deref(),
            Some("10")
        );
        // Insert 5 bytes at offset 0 → the marker shifts to 15.
        engine.adjust_markers(0, 0, 5);
        assert_eq!(
            engine.eval("(marker-position m)").unwrap().value.as_deref(),
            Some("15")
        );
        // Delete [2, 8) (6 bytes) → marker (15) shifts back by 6 to 9.
        engine.adjust_markers(2, 8, 0);
        assert_eq!(
            engine.eval("(marker-position m)").unwrap().value.as_deref(),
            Some("9")
        );
        // A marker inside a deleted span collapses to the deletion start.
        engine.eval("(define inside (make-marker 5))").unwrap();
        engine.adjust_markers(3, 7, 0);
        assert_eq!(
            engine
                .eval("(marker-position inside)")
                .unwrap()
                .value
                .as_deref(),
            Some("3")
        );
    }

    /// Minibuffer + process + macro + line-suffix commands parse.
    #[test]
    fn misc_primitives_parse() {
        let (engine, _rx) = ScriptEngine::new();
        // `completing-read` / `read-string` are now core.scm wrappers (optional
        // initial value) over the Rust `*-impl` primitives.
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        assert!(matches!(
            engine.eval(r#"(completing-read "Pick: " (list "a" "b") "cb")"#).unwrap().commands.as_slice(),
            [EditorCommand::CompletingRead { candidates, callback, .. }]
                if candidates == &["a", "b"] && callback == "cb"
        ));
        assert!(matches!(
            engine.eval(r#"(y-or-n-p "Sure?" "cb")"#).unwrap().commands.as_slice(),
            [EditorCommand::YesOrNo { callback, .. }] if callback == "cb"
        ));
        assert!(matches!(
            engine.eval(r#"(set-line-suffix 3 "// hint" "dim")"#).unwrap().commands.as_slice(),
            [EditorCommand::SetLineSuffix { line: 3, text, face }] if text == "// hint" && face == "dim"
        ));
        // make-process returns an id and enqueues SendProcess/KillProcess by id.
        assert!(matches!(
            engine
                .eval(r"(kill-process 1)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::KillProcess { id: 1 }]
        ));
        assert!(matches!(
            engine
                .eval(r#"(set-process-buffer 1 "*shell*")"#)
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::SetProcessBuffer { id: 1, name }] if name == "*shell*"
        ));
        assert!(matches!(
            engine
                .eval("(start-kbd-macro)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::StartKbdMacro]
        ));
    }

    /// The Scheme editing lib (edit.scm) loads and its pure helpers work.
    #[test]
    fn edit_lib_helpers() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/edit.scm")).unwrap();
        // uncomment-line strips the first comment prefix.
        assert_eq!(
            engine
                .eval(r#"(uncomment-line ";; hello" ";; ")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"hello\"")
        );
        // Default comment string (no buffer-local set) is "// ".
        assert_eq!(
            engine.eval("(comment-string)").unwrap().value.as_deref(),
            Some("\"// \"")
        );
    }

    /// M-; (comment-dwim) picks the leader by language (not always "//"),
    /// comments at the line's indentation, toggles back, and honors the
    /// buffer-local "comment-string" override.
    #[test]
    fn comment_dwim_uses_language_leader() {
        use EditorCommand as C;
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/edit.scm")).unwrap();
        let set = |lang: &str, text: &str, point: usize| {
            engine.set_snapshot(EditorSnapshot {
                text: Rope::from_str(text),
                point,
                selections: vec![(point, point)],
                language: lang.into(),
                major_mode: lang.into(),
                ..Default::default()
            });
        };
        let replaced = |cmds: &[C]| {
            cmds.iter()
                .find_map(|c| match c {
                    C::ReplaceRegion { start, end, text } => Some((*start, *end, text.clone())),
                    _ => None,
                })
                .expect("a ReplaceRegion")
        };
        set("python", "x = 1\n  y = 2\n", 8);
        let c = engine.eval("(comment-dwim)").unwrap().commands;
        assert_eq!(replaced(&c), (6, 13, "  # y = 2".to_string()));
        set("python", "  # y = 2\n", 4);
        let c = engine.eval("(comment-dwim)").unwrap().commands;
        assert_eq!(replaced(&c), (0, 9, "  y = 2".to_string()));
        set("lua", "x = 1", 0);
        let c = engine.eval("(comment-dwim)").unwrap().commands;
        assert_eq!(replaced(&c), (0, 5, "-- x = 1".to_string()));
        // Unknown languages fall back to "// ".
        set("rust", "x", 0);
        let c = engine.eval("(comment-dwim)").unwrap().commands;
        assert_eq!(replaced(&c), (0, 1, "// x".to_string()));
        // A buffer-local leader wins over the language table.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str("x"),
            language: "python".into(),
            major_mode: "python".into(),
            locals: vec![("comment-string".into(), "; ".into())],
            ..Default::default()
        });
        let c = engine.eval("(comment-dwim)").unwrap().commands;
        assert_eq!(replaced(&c), (0, 1, "; x".to_string()));
    }

    /// Async primitives parse to their commands.
    #[test]
    fn async_primitives_parse() {
        let (engine, _rx) = ScriptEngine::new();
        match engine
            .eval(r#"(make-process "grep" (list "-rn" "x" ".") "on-line" "on-done")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::MakeProcess(p)] => {
                assert_eq!(p.program, "grep");
                assert_eq!(p.args, ["-rn", "x", "."]);
                assert_eq!(p.on_output, "on-line");
                assert_eq!(p.on_exit.as_deref(), Some("on-done"));
                assert!(p.env.0.is_empty(), "no extent open → no env");
            }
            other => panic!("expected MakeProcess, got {other:?}"),
        }
        // Empty on-exit string means "no exit callback".
        match engine
            .eval(r#"(make-process "ls" (list) "on-line" "")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::MakeProcess(p)] => assert!(p.on_exit.is_none()),
            other => panic!("expected MakeProcess, got {other:?}"),
        }
        assert!(matches!(
            engine.eval(r#"(run-with-timer 500 "tick")"#).unwrap().commands.as_slice(),
            [EditorCommand::RunWithTimer { ms: 500, callback }] if callback == "tick"
        ));
        // make-http-stream pairs its flat key/value header list and keeps body +
        // callbacks intact.
        match engine
            .eval(
                r#"(make-http-stream "https://api.anthropic.com/v1/messages"
                      (list "x-api-key" "sk-test" "anthropic-version" "2023-06-01")
                      "{\"stream\":true}" "on-line" "on-done")"#,
            )
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::HttpStream(args)] => {
                let HttpStreamArgs {
                    url,
                    headers,
                    body,
                    on_line,
                    on_done,
                    ..
                } = args.as_ref();
                assert_eq!(url, "https://api.anthropic.com/v1/messages");
                assert_eq!(
                    headers.as_slice(),
                    [
                        ("x-api-key".to_string(), "sk-test".to_string()),
                        ("anthropic-version".to_string(), "2023-06-01".to_string()),
                    ]
                );
                assert_eq!(body, r#"{"stream":true}"#);
                assert_eq!(on_line, "on-line");
                assert_eq!(on_done, "on-done");
            }
            other => panic!("expected HttpStream, got {other:?}"),
        }
        assert!(matches!(
            engine
                .eval(r"(http-stream-cancel 7)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::HttpStreamCancel { id: 7 }]
        ));
    }

    /// `(make-terminal name program args directory)` enqueues a `MakeTerminal`
    /// with the requested launch directory and returns a fresh id.
    #[test]
    fn make_terminal_parses() {
        let (engine, _rx) = ScriptEngine::new();
        match engine
            .eval(r#"(make-terminal "*terminal*" "/bin/fish" (list "-l") "/w/proj")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::MakeTerminal(t)] => {
                assert_eq!(t.name, "*terminal*");
                assert_eq!(t.program, "/bin/fish");
                assert_eq!(t.args, ["-l"]);
                assert_eq!(t.directory, "/w/proj");
                assert!(t.id > 0);
            }
            other => panic!("expected MakeTerminal, got {other:?}"),
        }
    }

    /// `(transient title rows)` enqueues a `Transient` with parsed rows.
    #[test]
    fn transient_parses_rows() {
        let (engine, _rx) = ScriptEngine::new();
        let result = engine
            .eval(r#"(transient "Commit" (list (list "c" "Commit" "magit-do-commit")))"#)
            .unwrap();
        match result.commands.as_slice() {
            // Flat form → a single untitled column.
            [EditorCommand::Transient { title, columns, .. }] => {
                assert_eq!(title, "Commit");
                assert_eq!(columns.len(), 1);
                assert_eq!(columns[0].title, None);
                let rows = &columns[0].rows;
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].key, "c");
                assert_eq!(rows[0].command, "magit-do-commit");
            }
            other => panic!("expected one Transient, got {other:?}"),
        }
    }

    #[test]
    fn transient_parses_columns() {
        let (engine, _rx) = ScriptEngine::new();
        // Grouped form → titled columns.
        let result = engine
            .eval(
                r#"(transient "Menu"
                     (list (list "Edit" (list "c" "Commit" "do-c") (list "a" "Amend" "do-a"))
                           (list "Sync" (list "p" "Push" "do-p"))))"#,
            )
            .unwrap();
        match result.commands.as_slice() {
            [EditorCommand::Transient { columns, .. }] => {
                assert_eq!(columns.len(), 2);
                assert_eq!(columns[0].title.as_deref(), Some("Edit"));
                assert_eq!(columns[0].rows.len(), 2);
                assert_eq!(columns[1].title.as_deref(), Some("Sync"));
                assert_eq!(columns[1].rows[0].key, "p");
            }
            other => panic!("expected one Transient, got {other:?}"),
        }
    }

    /// The Scheme libs load (no git runs at load — only defines), and the pure
    /// helpers magit relies on (named-let lookup, porcelain parsing, transient
    /// arg toggles) behave.
    #[test]
    fn lsp_config_wrapper_registers_languages_and_formatters() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(include_str!("../../scheme/core.scm"))
            .expect("core.scm");
        engine
            .eval(include_str!("../../scheme/lsp.scm"))
            .expect("lsp.scm");

        // plist + extension helpers.
        assert_eq!(
            engine
                .eval("(lsp--plist-get (list 'a 1 'b 2) 'b)")
                .unwrap()
                .value
                .as_deref(),
            Some("2")
        );
        assert_eq!(
            engine
                .eval(r#"(lsp--file-ext "src/main.rs")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"rs\"")
        );
        assert_eq!(
            engine
                .eval(r#"(lsp--file-ext "Makefile")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"\"")
        );

        // A shell formatter registers against every extension of the language.
        engine
            .eval(
                r#"(lsp-language "rust" 'extensions (list "rs") 'format-on-save (list "rustfmt"))"#,
            )
            .expect("lsp-language");
        assert_eq!(
            engine
                .eval(r#"(car (alist-ref *lsp-formatters* "rs"))"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"rustfmt\"")
        );

        // 'lsp format-on-save enqueues the Rust toggle command, not a shell spec.
        let cmds = engine
            .eval(r#"(lsp-language "go" 'extensions (list "go") 'format-on-save 'lsp)"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::SetLspFormatOnSave(l, true) if l == "go")),
            "'lsp format-on-save should enable the Rust path: {cmds:?}"
        );
        // …and registers no shell formatter for .go.
        assert_eq!(
            engine
                .eval(r#"(alist-ref *lsp-formatters* "go")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#false")
        );
    }

    #[test]
    fn scheme_libs_load_and_helpers_work() {
        let (engine, _rx) = ScriptEngine::new();
        // Load the full bundled library set in real startup order (sorted by
        // name), exactly as production does. Cherry-picking individual files is
        // brittle: e.g. projectile.scm references persistence-dir (defined in
        // persistence.scm), which in turn pulls in helm.scm — and Steel resolves
        // such free identifiers at load time. Loading everything mirrors startup
        // and keeps this test stable as libs gain cross-dependencies.
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script");
        }

        // core.scm helpers.
        assert_eq!(
            engine
                .eval(r#"(basename "/home/me/proj/")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"proj\"")
        );
        assert_eq!(
            engine
                .eval(r#"(path-join "/a/b" "c.rs")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"/a/b/c.rs\"")
        );
        assert_eq!(
            engine
                .eval(r#"(alist-ref (list (cons "k" "v")) "k")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"v\"")
        );

        // steel-mode.scm registers an interactive `steel-mode` command so a
        // buffer can be switched into the Steel keymap by hand (M-x steel-mode),
        // mirroring the editor-aware `macros-steel-mode` builtin.
        assert_eq!(
            engine
                .eval(r#"(if (member "steel-mode" *interactive-commands*) "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"y\"")
        );

        // save-excursion macro expands and evaluates (point/goto-char are stubbed
        // by the default snapshot here, but the form must compile and run).
        engine
            .eval(r"(save-excursion (+ 1 2))")
            .expect("save-excursion");

        // The `fuzzy-match` primitive ranks/filters from Scheme (the building
        // block for a Scheme-driven Helm): the non-match drops, tighter first.
        assert_eq!(
            engine
                .eval(r#"(car (fuzzy-match "fbr" (list "unrelated.txt" "foo/bar.rs")))"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"foo/bar.rs\"")
        );

        // projectile root resolution walks up to a marker on the real FS. This
        // crate's dir contains Cargo.toml, so it resolves to the manifest dir.
        let dir = env!("CARGO_MANIFEST_DIR");
        let root = engine
            .eval(&format!("(project-root \"{dir}/src\")"))
            .unwrap()
            .value
            .unwrap();
        assert_eq!(root, format!("\"{dir}\""));

        // A nested git worktree/submodule (`.git` FILE) is its own project even
        // inside another repo (`.git` DIRECTORY): the inner checkout wins.
        let base = std::env::temp_dir().join(format!("macros-wt-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let inner = base.join("wt").join("inner");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(inner.join("src")).unwrap();
        std::fs::write(inner.join(".git"), "gitdir: ../../.git/worktrees/inner\n").unwrap();
        let (base_s, inner_s) = (base.display(), inner.display());
        let root = engine
            .eval(&format!("(project-root \"{inner_s}/src\")"))
            .unwrap()
            .value
            .unwrap();
        assert_eq!(root, format!("\"{inner_s}\""));
        // A plain sub-dir of the outer repo still resolves to the outer root.
        std::fs::create_dir_all(base.join("other")).unwrap();
        let root = engine
            .eval(&format!("(project-root \"{base_s}/other\")"))
            .unwrap()
            .value
            .unwrap();
        assert_eq!(root, format!("\"{base_s}\""));
        let _ = std::fs::remove_dir_all(&base);

        // Porcelain entry parsing.
        assert_eq!(
            engine
                .eval(r#"(entry-path " M src/foo.rs")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"src/foo.rs\"")
        );
        assert_eq!(
            engine
                .eval(r#"(entry-status "?? new.txt")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"??\"")
        );

        // Section-at-point lookup against fixed *magit-sections*
        // (heading path type body-start body-end). Line 4 is inside b.rs's body.
        engine
            .eval(
                r#"(set! *magit-sections*
                     (list (list 0 "a.rs" "staged" 1 1)
                           (list 3 "b.rs" "unstaged" 4 7)))"#,
            )
            .unwrap();
        assert_eq!(
            engine
                .eval("(sec-path (magit-section-at-line 5))")
                .unwrap()
                .value
                .as_deref(),
            Some("\"b.rs\"")
        );
        // The heading line itself also resolves to its section.
        assert_eq!(
            engine
                .eval("(sec-path (magit-section-at-line 0))")
                .unwrap()
                .value
                .as_deref(),
            Some("\"a.rs\"")
        );

        // Transient argument toggles.
        engine
            .eval(r#"(transient-toggle-arg "--force" (lambda () #f))"#)
            .unwrap();
        assert_eq!(
            engine
                .eval(r#"(transient-arg-active? "--force")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true")
        );
        engine
            .eval(r#"(transient-toggle-arg "--force" (lambda () #f))"#)
            .unwrap();
        assert_eq!(
            engine
                .eval(r#"(transient-arg-active? "--force")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#false")
        );
    }

    /// Structural diff view: funcname-context extraction and hunk grouping are
    /// pure reshapings of the line hunks (no git, no AST), the grouped emitter
    /// lays out sub-headings + foldable spans, and group lookup over
    /// *magit-groups* distinguishes a heading (whole-fn staging) from a body
    /// line (fold). The view itself is a live global, line view by default.
    #[test]
    fn structural_diff_grouping() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script failed to load");
        }
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();

        // Context = text after the second "@@" on the header, trimmed; the rest
        // of the hunk body is ignored (only the first line is read).
        assert_eq!(
            v(r#"(magit-hunk-context "@@ -10,7 +10,8 @@ fn parse_expr(&mut self) {\n+  x")"#),
            "\"fn parse_expr(&mut self) {\""
        );
        // No funcname (top-of-file change, or a language with no pattern) → ""
        // → displayed as "(top level)".
        assert_eq!(v(r#"(magit-hunk-context "@@ -1,3 +1,4 @@\n+x")"#), "\"\"");
        assert_eq!(v(r#"(magit-group-display "")"#), "\"(top level)\"");

        // Consecutive same-context hunks merge; a new context starts a new group.
        engine
            .eval(
                r#"(define gtest
                     (magit-group-hunks
                       (list "@@ -1,2 +1,2 @@ fn a\n x"
                             "@@ -9,2 +9,2 @@ fn a\n y"
                             "@@ -20,2 +20,2 @@ fn b\n z")))"#,
            )
            .unwrap();
        assert_eq!(v("(length gtest)"), "2"); // two groups
        assert_eq!(v("(car (car gtest))"), "\"fn a\""); // first context
        assert_eq!(v("(length (cdr (car gtest)))"), "2"); // fn a holds 2 hunks
        assert_eq!(v("(car (cadr gtest))"), "\"fn b\""); // second context

        // Group lookup over a fixed *magit-groups*
        // (heading body-start body-end type path context).
        engine
            .eval(r#"(set! *magit-groups* (list (list 5 6 9 "unstaged" "a.rs" "fn a")))"#)
            .unwrap();
        // A body line resolves to its group (for TAB fold)…
        assert_eq!(v("(grp-context (magit-group-at-line 6))"), "\"fn a\"");
        // …but only the heading line is a staging target (s/u/k on a whole fn).
        assert_eq!(v("(grp-context (magit-group-on-heading 5))"), "\"fn a\"");
        assert_eq!(v(r#"(if (magit-group-on-heading 6) "y" "n")"#), "\"n\"");

        // End-to-end render path: drive the grouped emitter over a 3-hunk diff
        // (fn a ×2, fn b ×1) on a reset of the render globals, then inspect what
        // it laid down — group count, the fn-a sub-heading text and its foldable
        // body span, and that every hunk is still registered for per-hunk ops.
        engine
            .eval(
                r#"(begin
                     (section-begin)
                     (set! *magit-groups* '()) (set! *magit-hunks* '())
                     (magit-emit-hunks-grouped "hdr"
                       (list "@@ -1,2 +1,2 @@ fn a\n ctx\n+a1"
                             "@@ -9,2 +9,2 @@ fn a\n ctx\n+a2"
                             "@@ -20,2 +20,2 @@ fn b\n ctx\n+b1")
                       "unstaged" "a.rs"))"#,
            )
            .unwrap();
        assert_eq!(v("(length *magit-groups*)"), "2"); // fn a, fn b
        assert_eq!(v("(length *magit-hunks*)"), "3"); // all hunks still per-hunk
        // The fn-a group: heading at line 0, body lines 1..7 (heading + 2×3-line
        // hunks), and the sub-heading text reads "(2 hunks)".
        assert_eq!(v("(grp-context (magit-group-at-line 0))"), "\"fn a\"");
        assert_eq!(v("(grp-body-start (magit-group-at-line 0))"), "1");
        assert_eq!(v("(grp-body-end (magit-group-at-line 0))"), "7");
        assert_eq!(
            v("(list-ref (reverse *section-lines*) 0)"),
            "\"    fn a  (2 hunks)\""
        );
        // fn b starts where fn a's body ends.
        assert_eq!(v("(grp-context (magit-group-at-line 7))"), "\"fn b\"");

        // The view is a live global (so a toggle takes effect on the next
        // render), line view by default.
        assert_eq!(v("(magit-structural?)"), "#false");
        engine
            .eval(r#"(set! *magit-diff-view* "structural")"#)
            .unwrap();
        assert_eq!(v("(magit-structural?)"), "#true");
    }

    /// Within-line word diff: common prefix/suffix trimming locates the changed
    /// middle of a modified line, and the emit path paints exactly that span on
    /// the word faces (leaving the rest of the line on the dim body face). The
    /// hunk TEXT is never touched — this is overlay-only.
    #[test]
    fn word_diff_highlights_changed_span() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script failed to load");
        }
        let v = |e: &str| engine.eval(e).unwrap().value.unwrap_or_default();

        // Prefix/suffix counts (char-indexed).
        assert_eq!(v(r#"(magit-common-prefix "abc" "abx")"#), "2");
        assert_eq!(v(r#"(magit-common-suffix "abc" "xbc" 3)"#), "2");
        // The changed middle of a vs b, as a (start . end) char range. Here only
        // the "-" vs " " at index 8 differs → a one-char span.
        assert_eq!(
            v(r#"(car (magit-change-span "An Emacs-style" "An Emacs style"))"#),
            "8"
        );
        assert_eq!(
            v(r#"(cdr (magit-change-span "An Emacs-style" "An Emacs style"))"#),
            "9"
        );
        // A wholly-shared side (pure insertion on the other) yields an empty span.
        assert_eq!(v(r#"(car (magit-change-span "foo" "foobar"))"#), "3");
        assert_eq!(v(r#"(cdr (magit-change-span "foo" "foobar"))"#), "3");

        // End-to-end: emit a paired -/+ body on a byte-offset reset, then check
        // the accents. Line 0 "-An Emacs-style, x" (18 bytes) sits at byte 0, so
        // line 1 "+An Emacs style, x" starts at byte 19. The changed char is at
        // content index 8 → byte 9 (removed) / byte 28 (added), one byte wide.
        engine
            .eval(
                r#"(begin
                     (section-begin)
                     (magit-emit-hunk-body
                       (list "-An Emacs-style, x" "+An Emacs style, x")))"#,
            )
            .unwrap();
        // The whole modified line sits on the DIM base face (only the change is
        // re-brightened below), so unchanged text recedes.
        assert_eq!(
            v(r#"(if (member (list 0 "magit-removed-dim") *section-faces*) "y" "n")"#),
            "\"y\""
        );
        assert_eq!(
            v(r#"(if (member (list 1 "magit-added-dim") *section-faces*) "y" "n")"#),
            "\"y\""
        );
        // The removed "-" and the added " " are each painted on their word face…
        assert_eq!(
            v(r#"(if (member (list 9 10 "magit-word-removed") *section-accents*) "y" "n")"#),
            "\"y\""
        );
        assert_eq!(
            v(r#"(if (member (list 28 29 "magit-word-added") *section-accents*) "y" "n")"#),
            "\"y\""
        );

        // Unbalanced blocks (2 removed, 1 added) are NOT paired — no word accent.
        engine
            .eval(
                r#"(begin
                     (section-begin)
                     (magit-emit-hunk-body (list "-aaa" "-bbb" "+ccc")))"#,
            )
            .unwrap();
        // "y" = no word-face accent was recorded for the unbalanced block.
        assert_eq!(
            v(
                r#"(if (null? (filter (lambda (a) (or (equal? (nth a 2) "magit-word-removed")
                                                    (equal? (nth a 2) "magit-word-added")))
                                    *section-accents*)) "y" "n")"#
            ),
            "\"y\""
        );
    }
}

#[cfg(test)]
mod search_e2e {
    use super::*;

    #[test]
    fn walk_and_grep_respect_gitignore_and_config() {
        let dir = std::env::temp_dir().join(format!("macros-search-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        let d = dir.to_str().unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .arg("init")
            .arg("-q")
            .output()
            .unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("a.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(dir.join("b.txt"), "plain\n").unwrap();
        std::fs::write(dir.join("sub").join("c.rs"), "fn c() {}\n").unwrap();
        std::fs::write(dir.join("target").join("junk.rs"), "ignored\n").unwrap();
        std::fs::write(dir.join(".secret"), "hidden\n").unwrap();

        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval("(define (any-ends? lst suf) (if (null? lst) #f (if (ends-with? (car lst) suf) #t (any-ends? (cdr lst) suf))))")
            .unwrap();
        let walks = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // Default walk: real files in, gitignored + hidden out.
        let w = format!("(walk-files {d:?})");
        assert_eq!(walks(&format!("(any-ends? {w} \"a.rs\")")), "#true");
        assert_eq!(walks(&format!("(any-ends? {w} \"sub/c.rs\")")), "#true");
        assert_eq!(
            walks(&format!("(any-ends? {w} \"junk.rs\")")),
            "#false",
            "gitignored"
        );
        assert_eq!(
            walks(&format!("(any-ends? {w} \".secret\")")),
            "#false",
            "hidden"
        );

        // Configurable exclude.
        engine.eval(r#"(search-exclude "*.txt")"#).unwrap();
        assert_eq!(
            walks(&format!("(any-ends? {w} \"b.txt\")")),
            "#false",
            "excluded glob"
        );
        // Configurable include (whitelist) — only .rs after reset.
        engine.eval("(search-reset)").unwrap();
        engine.eval(r#"(search-include "*.rs")"#).unwrap();
        assert_eq!(
            walks(&format!("(any-ends? {w} \"b.txt\")")),
            "#false",
            "not whitelisted"
        );
        assert_eq!(walks(&format!("(any-ends? {w} \"a.rs\")")), "#true");
        // Hidden opt-in surfaces .secret.
        engine.eval("(search-reset)").unwrap();
        engine
            .eval(r#"(set-search-option "hidden" "true")"#)
            .unwrap();
        assert_eq!(walks(&format!("(any-ends? {w} \".secret\")")), "#true");

        // grep-files finds the needle and skips ignored dirs.
        engine.eval("(search-reset)").unwrap();
        let g = format!(r#"(grep-files {d:?} "needle")"#);
        assert_eq!(walks(&format!("(length {g})")), "1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_root_prefers_outermost_vcs_root() {
        // Repo root has .git; a nested member crate has its own Cargo.toml.
        // Resolution from inside the member must return the repo root, not the
        // member dir, so workspace members don't show up as separate projects.
        let dir = std::env::temp_dir().join(format!("macros-projroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates/member/src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // marker presence is enough
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(dir.join("crates/member/Cargo.toml"), "[package]\n").unwrap();
        let d = dir.to_str().unwrap();

        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("startup script");
        }
        let root = engine
            .eval(&format!("(project-root \"{d}/crates/member/src\")"))
            .unwrap()
            .value
            .unwrap();
        assert_eq!(root, format!("\"{d}\""), "member resolves to repo root");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod magit_e2e {
    use super::*;
    #[test]
    fn magit_render_parses_real_diffs() {
        // Build a temp repo with a staged add + an unstaged modification.
        let dir = std::env::temp_dir().join(format!("macros-magit-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "one\nCHANGED\n").unwrap(); // unstaged mod
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        git(&["add", "b.txt"]); // staged add

        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        engine.eval(&format!("(set! *magit-dir* {d:?})")).unwrap();

        // Bodies are lazy: an all-collapsed render parses no hunks at all.
        engine.eval("(magit-render)").unwrap();
        assert_eq!(
            engine
                .eval("(length *magit-hunks*)")
                .unwrap()
                .value
                .as_deref(),
            Some("0"),
            "collapsed render builds no hunk bodies"
        );

        // Expand a.txt (unstaged) → its diff body renders on the next render.
        engine
            .eval(r#"(set! *magit-expanded* (list (cons "unstaged" "a.txt")))"#)
            .unwrap();
        engine.eval("(magit-render)").unwrap();

        // Both files appear as sections; the unstaged one has a real diff body.
        assert!(
            engine
                .eval("(length *magit-sections*)")
                .unwrap()
                .value
                .unwrap()
                .parse::<i64>()
                .unwrap()
                >= 2
        );
        // a.txt (unstaged) has body lines (body-end > body-start) and accents.
        let accents = engine
            .eval("(length *section-accents*)")
            .unwrap()
            .value
            .unwrap();
        assert!(
            accents.parse::<i64>().unwrap() >= 2,
            "expected +/- accents, got {accents}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod magit_hunk_e2e {
    use super::*;
    #[test]
    fn hunk_staging_against_real_git() {
        let dir = std::env::temp_dir().join(format!("macros-hunk-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("f.txt"), "a\nCHANGED\nc\nd\n").unwrap(); // one unstaged hunk

        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        engine.eval(&format!("(set! *magit-dir* {d:?})")).unwrap();
        // Bodies are lazy — expand f.txt so its hunks are parsed + recorded.
        engine
            .eval(r#"(set! *magit-expanded* (list (cons "unstaged" "f.txt")))"#)
            .unwrap();
        engine.eval("(magit-render)").unwrap();

        // The diff parsed into at least one hunk with a header + @@ text.
        assert!(
            engine
                .eval("(length *magit-hunks*)")
                .unwrap()
                .value
                .unwrap()
                .parse::<i64>()
                .unwrap()
                >= 1
        );
        assert_eq!(
            engine
                .eval(r#"(starts-with? (nth (car *magit-hunks*) 2) "diff --git")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "hunk carries the file header"
        );

        // Stage the hunk → it moves to the index, nothing left unstaged.
        assert_eq!(
            engine
                .eval("(magit-apply-hunk (car *magit-hunks*) #t #f)")
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "git apply --cached succeeded"
        );
        assert_eq!(
            engine
                .eval(r#"(string-contains? (git (list "diff" "--cached" "--name-only")) "f.txt")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "the hunk is now staged"
        );
        assert_eq!(
            engine
                .eval(r#"(equal? (trim-end (git (list "diff" "--name-only"))) "")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "nothing left unstaged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn section_header_staging_against_real_git() {
        let dir = std::env::temp_dir().join(format!("macros-section-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        // Committed file we then modify (→ unstaged), plus two untracked files.
        std::fs::write(dir.join("a.txt"), "a\nb\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "a\nCHANGED\n").unwrap(); // unstaged mod
        std::fs::write(dir.join("u1.txt"), "1\n").unwrap(); // untracked
        std::fs::write(dir.join("u2.txt"), "2\n").unwrap(); // untracked

        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        engine.eval(&format!("(set! *magit-dir* {d:?})")).unwrap();
        engine.eval("(magit-render)").unwrap();

        // Each section header was recorded with its type, and the line→type
        // lookup resolves on the exact header line.
        let untracked_head = engine
            .eval("(car (filter (lambda (h) (equal? (cdr h) \"untracked\")) *magit-headings*))")
            .unwrap();
        assert!(
            untracked_head.value.is_some(),
            "an untracked header line was recorded"
        );
        assert_eq!(
            engine
                .eval(
                    "(magit-heading-type-at-line \
                       (car (car (filter (lambda (h) (equal? (cdr h) \"untracked\")) \
                                         *magit-headings*))))"
                )
                .unwrap()
                .value
                .as_deref(),
            Some("\"untracked\""),
            "the recorded untracked header line resolves back to \"untracked\""
        );

        // An untracked file's body comes straight from read-file (no
        // subprocess): a synthesized @@ header plus "+"-prefixed lines.
        assert_eq!(
            engine
                .eval(r#"(string-join (magit-untracked-diff "u1.txt") "|")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"@@ -0,0 +1,1 @@|+1\""),
            "untracked body is read + rendered as added lines"
        );

        // `s` on the "Untracked files" header stages every untracked file.
        assert_eq!(
            engine
                .eval(r#"(length (magit-files-of-type "untracked"))"#)
                .unwrap()
                .value
                .as_deref(),
            Some("2"),
            "both untracked files are in the section"
        );
        engine.eval(r#"(magit-stage-section "untracked")"#).unwrap();
        assert_eq!(
            engine
                .eval(r#"(equal? (trim-end (git (list "ls-files" "--others" "--exclude-standard"))) "")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "no untracked files remain after staging the section"
        );
        for f in ["u1.txt", "u2.txt"] {
            assert_eq!(
                engine
                    .eval(&format!(
                        r#"(string-contains? (git (list "diff" "--cached" "--name-only")) "{f}")"#
                    ))
                    .unwrap()
                    .value
                    .as_deref(),
                Some("#true"),
                "{f} is now staged"
            );
        }

        // `s` on the "Unstaged changes" header stages every unstaged change.
        engine.eval("(magit-render)").unwrap();
        engine.eval(r#"(magit-stage-section "unstaged")"#).unwrap();
        assert_eq!(
            engine
                .eval(r#"(equal? (trim-end (git (list "diff" "--name-only"))) "")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "nothing left unstaged after staging the section"
        );

        // `u` on the "Staged changes" header unstages the whole section.
        engine.eval("(magit-render)").unwrap();
        engine.eval("(magit-unstage-section)").unwrap();
        assert_eq!(
            engine
                .eval(r#"(equal? (trim-end (git (list "diff" "--cached" "--name-only"))) "")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("#true"),
            "nothing left staged after unstaging the section"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gix_e2e {
    use super::*;
    #[test]
    fn blame_log_and_toplevel_against_real_git() {
        let dir = std::env::temp_dir().join(format!("macros-gix-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        // Commit 1 "init": original two lines.
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "init"]);
        // Commit 2 "second": rewrite line 2 and append a third line.
        std::fs::write(dir.join("f.txt"), "one\nTWO\nthree\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "second"]);
        // Pin the branch name so the merge step below is git-version-independent.
        git(&["branch", "-M", "main"]);

        let (engine, _rx) = ScriptEngine::new();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // gix-toplevel resolves the work-tree root; "" for a non-repo path.
        assert_eq!(
            v(&format!("(ends-with? (gix-toplevel {d:?}) {name:?})")),
            "#true",
            "toplevel points at the repo root"
        );
        assert_eq!(
            v(r#"(equal? (gix-toplevel "/no/such/repo/xyz") "")"#),
            "#true",
            "non-repo path yields empty string"
        );

        // gix-branch returns the short checked-out branch; "" outside a repo.
        assert_eq!(
            v(&format!(r#"(equal? (gix-branch {d:?}) "main")"#)),
            "#true",
            "branch reports the checked-out branch name"
        );
        assert_eq!(
            v(r#"(equal? (gix-branch "/no/such/repo/xyz") "")"#),
            "#true",
            "non-repo path yields empty branch"
        );

        // gix-log: two commits, newest first.
        assert_eq!(v(&format!("(length (gix-log {d:?} 10))")), "2");
        assert_eq!(
            v(&format!(
                r#"(string-contains? (list-ref (gix-log {d:?} 10) 0) "second")"#
            )),
            "#true",
            "newest commit first"
        );
        assert_eq!(
            v(&format!(
                r#"(string-contains? (list-ref (gix-log {d:?} 10) 1) "init")"#
            )),
            "#true",
            "oldest commit last"
        );
        // The count caps at `n`.
        assert_eq!(v(&format!("(length (gix-log {d:?} 1))")), "1");

        // HEAD subject / full message, and oneline log for the recent-commits view.
        assert_eq!(v(&format!("(gix-head-subject {d:?})")), "\"second\"");
        // Full message keeps git's trailing newline, like `log -1 --format=%B`.
        assert_eq!(v(&format!("(gix-head-message {d:?})")), "\"second\\n\"");
        assert_eq!(v(&format!("(length (gix-log-oneline {d:?} 10))")), "2");
        assert_eq!(
            v(&format!(
                r#"(string-contains? (list-ref (gix-log-oneline {d:?} 10) 0) "second")"#
            )),
            "#true",
            "oneline newest first"
        );

        // gix-blame: one row per committed line, attributed to the right commit.
        let blame = format!(r#"(gix-blame {d:?} "f.txt")"#);
        assert_eq!(v(&format!("(length {blame})")), "3", "three lines blamed");
        // Line 1 ("one") is unchanged since "init".
        assert_eq!(
            v(&format!(r#"(starts-with? (list-ref {blame} 0) "1\t")"#)),
            "#true"
        );
        assert_eq!(
            v(&format!(
                r#"(string-contains? (list-ref {blame} 0) "init")"#
            )),
            "#true",
            "line 1 came from the init commit"
        );
        // Lines 2 and 3 were introduced by "second".
        assert_eq!(
            v(&format!(r#"(starts-with? (list-ref {blame} 1) "2\t")"#)),
            "#true"
        );
        assert_eq!(
            v(&format!(
                r#"(string-contains? (list-ref {blame} 1) "second")"#
            )),
            "#true",
            "line 2 was rewritten by the second commit"
        );
        assert_eq!(
            v(&format!(
                r#"(string-contains? (list-ref {blame} 2) "second")"#
            )),
            "#true",
            "line 3 was added by the second commit"
        );
        // Rich row shape: LINE \t SHA \t AUTHOR \t EPOCH \t SUMMARY (5 fields),
        // with a numeric author-time epoch in field 3.
        assert_eq!(
            v(&format!(
                r#"(length (split-many (list-ref {blame} 0) "\t"))"#
            )),
            "5"
        );
        assert_eq!(
            v(&format!(
                r#"(if (string->number (list-ref (split-many (list-ref {blame} 0) "\t") 3)) #t #f)"#
            )),
            "#true",
            "field 3 is a Unix epoch"
        );
        // Alignment guard: a working-tree edit makes gix bail (magit then falls
        // back to git blame); restoring the file re-enables gix.
        std::fs::write(dir.join("f.txt"), "one\nDIRTY\nthree\n").unwrap();
        assert_eq!(
            v(&format!("(length {blame})")),
            "0",
            "modified file → gix yields nothing (git fallback)"
        );
        std::fs::write(dir.join("f.txt"), "one\nTWO\nthree\n").unwrap(); // restore to HEAD
        assert_eq!(
            v(&format!("(length {blame})")),
            "3",
            "clean again → gix blames"
        );

        // Ref listings: add a branch, a tag, and a remote, then check gix sees them.
        git(&["branch", "feature"]);
        git(&["tag", "v1"]);
        git(&["remote", "add", "origin", "https://example.com/r.git"]);
        assert_ne!(
            v(&format!(r#"(member "feature" (gix-branches {d:?}))"#)),
            "#false",
            "feature branch listed"
        );
        assert_ne!(
            v(&format!(r#"(member "v1" (gix-tags {d:?}))"#)),
            "#false",
            "tag listed"
        );
        assert_ne!(
            v(&format!(r#"(member "origin" (gix-remotes {d:?}))"#)),
            "#false",
            "remote listed"
        );
        // Clean repo: no in-progress merge or rebase.
        assert_eq!(v(&format!(r#"(gix-state {d:?})"#)), "\"\"");

        // gix-state reflects a real in-progress merge. Build a conflict.
        git(&["checkout", "-q", "-b", "other", "HEAD~1"]);
        std::fs::write(dir.join("f.txt"), "one\nOTHER\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "other-change"]);
        // Merging the divergent branches conflicts on line 2 → merge in progress.
        let mout = std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["merge", "main"])
            .output()
            .unwrap();
        assert!(!mout.status.success(), "merge should conflict");
        assert_eq!(
            v(&format!(r#"(gix-state {d:?})"#)),
            "\"merge\"",
            "gix-state sees the in-progress merge"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod magit_quiet_refresh {
    use super::*;
    // A background refresh (e.g. after-save while editing elsewhere) renders the
    // status buffer in place via render-buffer-quiet; the interactive path keeps
    // the focus-switching render.
    #[test]
    fn quiet_flag_selects_inplace_render() {
        let dir = std::env::temp_dir().join(format!("macros-magitq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "init"]);

        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        engine.eval(&format!("(set! *magit-dir* {d:?})")).unwrap();

        // Interactive: focus-switching render-buffer.
        let cmds = engine
            .eval("(begin (set! *magit-quiet* #f) (magit-render))")
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::RenderBuffer { name, quiet: false, .. } if name == "*magit*")
            ),
            "interactive refresh uses render-buffer"
        );
        // ...and pins the cursor's screen row so the view doesn't jump.
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::ScrollCursorToRow(_))),
            "interactive refresh restores the on-screen row"
        );

        // Background: quiet, in-place render — and NO overlay commands (those
        // target the active buffer, which during after-save is the file).
        let cmds = engine
            .eval("(begin (set! *magit-quiet* #t) (magit-render))")
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::RenderBuffer { name, quiet: true, .. } if name == "*magit*")
            ),
            "background refresh uses render-buffer-quiet"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::OverlayFace { .. })),
            "quiet refresh must not push overlays onto the active buffer"
        );
        // The background path doesn't touch scroll (the active buffer is the file).
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::ScrollCursorToRow(_))),
            "background refresh must not scroll the active (file) buffer"
        );

        // Every render (re-)declares a recursive watch on the repo root, so
        // external changes drive the live refresh.
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::WatchTree(owner, dirs) if owner == "magit" && dirs.iter().any(|p| p.contains(&name))
            )),
            "magit-render watches the repo root recursively"
        );

        // Toggling a FILE section re-renders (bodies are lazy — expanding must
        // build them) and pins the cursor's screen row via magit-render.
        std::fs::write(dir.join("a.txt"), "CHANGED\n").unwrap(); // unstaged mod
        engine.eval("(set! *magit-quiet* #f)").unwrap();
        engine.eval("(magit-render)").unwrap();
        // The test snapshot's point sits on line 0; alias it to a.txt's section
        // so magit-toggle-fold resolves a file heading there.
        engine
            .eval(r#"(set! *magit-sections* (list (list 0 "a.txt" "unstaged" 1 1)))"#)
            .unwrap();
        let cmds = engine.eval("(magit-toggle-fold)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::RenderBuffer { text, quiet: false, .. } if text.contains("+CHANGED")
            )),
            "expanding a lazy section re-renders with its diff body"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::ScrollCursorToRow(_))),
            "fold toggle restores the on-screen row"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gix_stash {
    use super::*;
    #[test]
    fn stash_list_and_view() {
        let dir = std::env::temp_dir().join(format!("macros-gixstash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        git(&["branch", "-M", "main"]);
        // No stashes yet → empty.
        assert!(gix_stash_list(d).unwrap().is_empty());
        // Two stashes.
        std::fs::write(dir.join("f.txt"), "edit1\n").unwrap();
        git(&["stash", "push", "-m", "first"]);
        std::fs::write(dir.join("f.txt"), "edit2\n").unwrap();
        git(&["stash", "push", "-m", "second"]);

        let list = gix_stash_list(d).unwrap();
        assert_eq!(list.len(), 2);
        assert!(
            list[0].starts_with("stash@{0}:"),
            "newest first: {}",
            list[0]
        );
        assert!(list[0].contains("second"), "stash@0 is the latest push");
        assert!(list[1].starts_with("stash@{1}:") && list[1].contains("first"));

        // The view path: gix-show must resolve `stash@{0}` (else RET on a stash
        // line would regress). If gix can't, this fails and we'd keep git.
        let show = gix_show(d, "stash@{0}").unwrap();
        assert!(show.starts_with("commit "), "gix-show resolves stash@{{0}}");

        // magit's stash picker gets the bare `stash@{N}` names from gix-stash-list,
        // matching `git stash list --format=%gd`.
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        engine.eval(&format!("(set! *magit-dir* {d:?})")).unwrap();
        assert_eq!(
            engine.eval("(magit-stash-refs)").unwrap().value.as_deref(),
            Some(r#"("stash@{0}" "stash@{1}")"#)
        );
        // `--format=%gd` output, for parity with what this replaced.
        let gd = git(&["stash", "list", "--format=%gd"]);
        assert_eq!(
            String::from_utf8_lossy(&gd.stdout),
            "stash@{0}\nstash@{1}\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gix_log_views {
    use super::*;
    #[test]
    fn decorated_and_range_logs() {
        let dir = std::env::temp_dir().join(format!("macros-gixlogv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "c1"]);
        git(&["branch", "-M", "main"]);
        git(&["tag", "v1"]);
        std::fs::write(dir.join("f.txt"), "2\n").unwrap();
        git(&["commit", "-qam", "c2"]);
        // Simulate an upstream behind HEAD by 1 commit.
        git(&["update-ref", "refs/remotes/origin/main", "HEAD~1"]);

        // Decorated log: newest first, HEAD/branch/tag decorations present.
        let log = gix_log_decorated(d, false, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert!(log[0].contains("c2"), "newest first");
        assert!(
            log[1].contains("HEAD -> main") || log[1].contains("tag: v1"),
            "decorations on the first commit: {}",
            log[1]
        );
        assert!(log[1].contains("tag: v1"), "tag decoration");

        // Range: commits in HEAD not in origin/main (the unpushed c2).
        let unpushed = gix_log_range(d, "HEAD", "refs/remotes/origin/main", 10).unwrap();
        assert_eq!(unpushed.len(), 1, "one unpushed commit");
        assert!(unpushed[0].contains("c2"));
        // Nothing the other direction.
        let unpulled = gix_log_range(d, "refs/remotes/origin/main", "HEAD", 10).unwrap();
        assert_eq!(unpulled.len(), 0);

        // File history: add a second file in c2; its log has only that commit,
        // while f.txt's log has both commits that touched it.
        std::fs::write(dir.join("other.txt"), "x\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "c3 other"]);
        let f_hist = gix_log_file(d, "f.txt", 50).unwrap();
        assert_eq!(f_hist.len(), 2, "f.txt touched by c1 and c2");
        let o_hist = gix_log_file(d, "other.txt", 50).unwrap();
        assert_eq!(o_hist.len(), 1, "other.txt touched only by c3");
        assert!(o_hist[0].contains("c3 other"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gix_status_view {
    use super::*;

    fn git(d: &str, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(args)
            .output()
            .unwrap()
    }

    #[test]
    fn status_matches_git_porcelain() {
        let dir = std::env::temp_dir().join(format!("macros-gixstat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        git(d, &["init", "-q"]);
        git(d, &["config", "user.email", "t@t"]);
        git(d, &["config", "user.name", "t"]);
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        std::fs::write(dir.join("d.txt"), "d1\n").unwrap();
        std::fs::write(dir.join("keep.txt"), "k1\n").unwrap();
        // Nested files: the head tree then has directory (tree) entries, which
        // must NOT leak into the status as bogus `D <dir>` rows.
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        std::fs::write(dir.join("sub/s.txt"), "s1\n").unwrap();
        std::fs::write(dir.join("sub/deep/x.txt"), "x1\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "base"]);
        git(d, &["branch", "-M", "main"]);

        // A spread of states: unstaged-M, staged-A, staged-D, untracked, ignored,
        // staged-then-modified (MM), plus a change and a delete inside subdirs.
        std::fs::write(dir.join("a.txt"), "a2\n").unwrap(); // " M"
        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        git(d, &["add", "b.txt"]); // "A "
        git(d, &["rm", "-q", "d.txt"]); // "D "
        std::fs::write(dir.join("c.txt"), "c1\n").unwrap(); // "??"
        std::fs::write(dir.join("noise.log"), "x\n").unwrap(); // ignored → absent
        std::fs::write(dir.join("keep.txt"), "k2\n").unwrap();
        git(d, &["add", "keep.txt"]);
        std::fs::write(dir.join("keep.txt"), "k3\n").unwrap(); // "MM"
        std::fs::write(dir.join("sub/s.txt"), "s2\n").unwrap(); // " M" in subdir
        git(d, &["rm", "-q", "sub/deep/x.txt"]); // "D " in deeper subdir
        std::fs::write(dir.join("sub/u.txt"), "u1\n").unwrap(); // "??" in subdir

        let out = git(
            d,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--branch",
                "--untracked-files=all",
                "--no-renames",
            ],
        );
        let git_lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let gix_lines = gix_status(d).unwrap();

        // Header (branch line) must match exactly.
        assert_eq!(gix_lines[0], git_lines[0], "branch header");
        // Entries match as sets (porcelain order isn't lexical; we sort).
        let mut g: Vec<String> = git_lines[1..].to_vec();
        let mut x: Vec<String> = gix_lines[1..].to_vec();
        g.sort();
        x.sort();
        assert_eq!(x, g, "porcelain entries");
        // The ignored file is in neither.
        assert!(!gix_lines.iter().any(|l| l.contains("noise.log")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_header_upstream_and_ahead() {
        let dir = std::env::temp_dir().join(format!("macros-gixstatb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        git(d, &["init", "-q"]);
        git(d, &["config", "user.email", "t@t"]);
        git(d, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "1\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "c1"]);
        git(d, &["branch", "-M", "main"]);
        std::fs::write(dir.join("f.txt"), "2\n").unwrap();
        git(d, &["commit", "-qam", "c2"]);
        // Configure an upstream a commit behind HEAD. `remote add` sets the fetch
        // refspec so the branch.merge→remote-tracking mapping resolves.
        git(d, &["remote", "add", "origin", "https://example.com/r.git"]);
        git(d, &["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
        git(d, &["branch", "--set-upstream-to=origin/main", "main"]);

        let out = git(d, &["status", "--porcelain", "--branch"]);
        let git_header = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(gix_status(d).unwrap()[0], git_header);
        assert!(git_header.contains("[ahead 1]"), "sanity: {git_header}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gix_show_view {
    use super::*;
    #[test]
    fn show_matches_git_for_header_and_diff() {
        let dir = std::env::temp_dir().join(format!("macros-gixshow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "Tester"]);
        std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        std::fs::write(dir.join("f.txt"), "a\nB\nc\n").unwrap();
        std::fs::write(dir.join("n.txt"), "new\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "second commit"]);

        let show = gix_show(d, "HEAD").unwrap();
        // Header: commit line, author, the subject (indented), and the diff.
        assert!(show.starts_with("commit "), "starts with commit <sha>");
        assert!(show.contains("Author: Tester <t@t>"));
        assert!(show.contains("    second commit"), "indented subject");
        assert!(
            show.contains("diff --git a/f.txt b/f.txt"),
            "modification diff"
        );
        assert!(show.contains("-b\n+B\n"), "the b→B change");
        assert!(show.contains("new file mode"), "n.txt addition vs parent");

        // The diff portion applies cleanly in reverse to a checkout of HEAD,
        // proving it really describes HEAD vs its parent.
        let start = show.find("diff --git").unwrap();
        let patch = dir.join("show.patch");
        std::fs::write(&patch, &show[start..]).unwrap();
        let ok = git(&["apply", "--check", "--reverse", patch.to_str().unwrap()])
            .status
            .success();
        assert!(ok, "show's diff is a valid patch against the worktree");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gix_diff_apply {
    use super::*;

    // The decisive gate: gix-generated diffs must round-trip through
    // `git apply --cached` (magit reuses the same text to stage hunks). Covers
    // multi-hunk modify, deletion, and the no-trailing-newline footgun.
    #[test]
    fn unstaged_diff_round_trips_through_git_apply() {
        let dir = std::env::temp_dir().join(format!("macros-gixdiff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap()
        };
        let runq = |args: &[&str]| {
            let o = git(args);
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).into_owned(),
            )
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        // 8-line file (room for two separated hunks), a file to delete, and a
        // file with NO trailing newline at EOF.
        std::fs::write(dir.join("f.txt"), "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n").unwrap();
        std::fs::write(dir.join("g.txt"), "g1\ng2\n").unwrap();
        std::fs::write(dir.join("h.txt"), b"no-newline-eof").unwrap(); // no \n
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);

        // Working-tree edits: two far-apart hunks in f, delete g, change h (still
        // no trailing newline → exercises the `\ No newline` marker on both sides).
        std::fs::write(
            dir.join("f.txt"),
            "l1\nCHANGED2\nl3\nl4\nl5\nl6\nCHANGED7\nl8\n",
        )
        .unwrap();
        std::fs::remove_file(dir.join("g.txt")).unwrap();
        std::fs::write(dir.join("h.txt"), b"changed-no-newline").unwrap(); // no \n

        let diff = gix_diff_text(d, false).unwrap();
        assert!(diff.contains("diff --git a/f.txt b/f.txt"));
        assert!(diff.contains("deleted file mode"), "g.txt deletion present");
        assert!(
            diff.contains("\\ No newline at end of file"),
            "no-newline marker emitted for h.txt"
        );

        // Apply the whole diff to the index. Must succeed.
        let patch = dir.join("gix.patch");
        std::fs::write(&patch, &diff).unwrap();
        let (ok, _) = runq(&["apply", "--cached", patch.to_str().unwrap()]);
        assert!(ok, "git apply --cached accepted the gix diff");

        // Everything moved to the index: nothing left unstaged, all three staged.
        let (_, unstaged) = runq(&["diff", "--name-only"]);
        assert_eq!(
            unstaged.trim(),
            "",
            "no unstaged changes remain after apply"
        );
        let (_, staged) = runq(&["diff", "--cached", "--name-only"]);
        for f in ["f.txt", "g.txt", "h.txt"] {
            assert!(staged.contains(f), "{f} is staged after apply");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Binary files must emit `Binary files … differ` with no body — never the
    // raw bytes (which garble magit's buffer and break its diff parsing).
    #[test]
    fn binary_files_emit_marker_not_content() {
        let dir = std::env::temp_dir().join(format!("macros-gixbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        // A binary blob (embedded NUL), a text file, and a nested file whose
        // parent directory we'll delete (so the head tree has a tree entry).
        std::fs::write(dir.join("logo.bin"), [0u8, 1, 2, 0, 3, 255, 0, 4]).unwrap();
        std::fs::write(dir.join("t.txt"), "hello\n").unwrap();
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/a.txt"), "1\n").unwrap();
        std::fs::write(dir.join("pkg/b.txt"), "2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        // Change the binary, add a new binary, and delete a whole directory.
        std::fs::write(dir.join("logo.bin"), [0u8, 9, 9, 9, 0, 1]).unwrap(); // unstaged binary change
        std::fs::write(dir.join("new.bin"), [1u8, 0, 2, 0, 3]).unwrap();
        git(&["add", "new.bin"]); // staged binary add
        git(&["rm", "-rq", "pkg"]); // staged delete of a directory's files

        let unstaged = gix_diff_text(d, false).unwrap();
        assert!(
            unstaged.contains("Binary files a/logo.bin and b/logo.bin differ"),
            "binary modification → marker, got:\n{unstaged}"
        );
        // No raw binary leaked as +/- content, and no spurious newline marker.
        assert!(!unstaged.contains("\\ No newline at end of file"));
        assert!(!unstaged.contains('\u{0}'), "no NUL bytes in the diff text");

        let staged = gix_diff_text(d, true).unwrap();
        assert!(
            staged.contains("Binary files /dev/null and b/new.bin differ"),
            "binary addition → marker, got:\n{staged}"
        );
        assert!(
            !staged.contains('\u{0}'),
            "no tree-object bytes leaked into staged diff"
        );
        // The directory delete shows its two files individually, never a `pkg` tree.
        assert!(staged.contains("a/pkg/a.txt") && staged.contains("a/pkg/b.txt"));
        assert!(
            !staged.lines().any(|l| l == "diff --git a/pkg b/pkg"),
            "no bare-directory diff entry"
        );
        // The diff stays parseable: exactly one `diff --git` per changed file.
        assert_eq!(
            unstaged.matches("diff --git").count(),
            1,
            "one file in unstaged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Staged diff (HEAD↔index) must describe the staged delta exactly: it
    // reverse-applies cleanly to the index, unstaging the change.
    #[test]
    fn cached_diff_matches_staged_delta() {
        let dir = std::env::temp_dir().join(format!("macros-gixdiffc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);

        // Stage a modification and a brand-new file.
        std::fs::write(dir.join("f.txt"), "a\nB\nc\n").unwrap();
        std::fs::write(dir.join("n.txt"), "new\nfile\n").unwrap();
        git(&["add", "-A"]);

        let cached = gix_diff_text(d, true).unwrap();
        assert!(cached.contains("--- a/f.txt"), "modification present");
        assert!(cached.contains("new file mode"), "n.txt addition present");

        // Reverse-applying the staged diff to the index unstages it cleanly.
        let patch = dir.join("cached.patch");
        std::fs::write(&patch, &cached).unwrap();
        let ok = git(&["apply", "--cached", "--reverse", patch.to_str().unwrap()])
            .status
            .success();
        assert!(ok, "staged diff reverse-applies to the index");
        let after = git(&["diff", "--cached", "--name-only"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&after).trim(),
            "",
            "index back to HEAD after reverse-apply"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod magit_blame_gix {
    use super::*;
    // magit's inline blame prefers gix and falls back to `git blame` only when
    // gix can't keep annotations aligned (a modified working-tree file).
    #[test]
    fn blame_prefers_gix_falls_back_to_git_when_dirty() {
        let dir = std::env::temp_dir().join(format!("macros-blame-gix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "init"]);

        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "magit");
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // Clean file → gix path: one record per line, attributed to a real commit.
        assert_eq!(
            v(&format!(r#"(length (magit-blame-fetch {d:?} "f.txt"))"#)),
            "3"
        );
        assert_eq!(
            v(&format!(
                r#"(magit-blame-uncommitted? (car (magit-blame-fetch {d:?} "f.txt")))"#
            )),
            "#false",
            "clean file blamed via gix → committed sha, not 0000000"
        );

        // Modify line 2 on disk (uncommitted): gix bails on the mismatch, so the
        // fetch falls back to `git blame`, which marks the line not-yet-committed.
        std::fs::write(dir.join("f.txt"), "a\nCHANGED\nc\n").unwrap();
        assert_eq!(
            v(&format!(
                r#"(magit-blame-uncommitted? (list-ref (magit-blame-fetch {d:?} "f.txt") 1))"#
            )),
            "#true",
            "modified line → git fallback flags it 0000000"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod helm_lib {
    use super::*;
    #[test]
    fn helm_occur_and_imenu_helpers() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // imenu name extraction across a few languages.
        assert_eq!(v(r#"(imenu-name "(define (foo x) ...)")"#), "\"foo\"");
        assert_eq!(v(r#"(imenu-name "fn bar() {")"#), "\"bar\"");
        assert_eq!(v(r#"(imenu-name "def baz(self):")"#), "\"baz\"");
        assert_eq!(v(r#"(imenu-name "class Qux:")"#), "\"Qux\"");
        assert_eq!(
            v(r#"(if (imenu-name "   x = 1") "hit" "none")"#),
            "\"none\""
        );
        // Occur rows are numbered 1-based like the gutter; picking "3: …"
        // lands on 0-based line 2.
        let cmds = engine
            .eval(r#"(helm-occur-goto "3: x = 1")"#)
            .unwrap()
            .commands;
        assert!(
            matches!(cmds.as_slice(), [EditorCommand::GotoLine(2)]),
            "{cmds:?}"
        );
        // new commands registered for M-x.
        for c in [
            "helm-buffers-list",
            "helm-mini",
            "list-buffers",
            "helm-recentf",
            "helm-occur",
            "helm-imenu",
        ] {
            assert_eq!(
                engine
                    .eval(&format!(
                        r#"(if (member "{c}" *interactive-commands*) "y" "n")"#
                    ))
                    .unwrap()
                    .value
                    .unwrap(),
                "\"y\"",
                "{c} not registered"
            );
        }
    }
}

#[cfg(test)]
mod diagnostics_lib {
    use super::*;
    #[test]
    fn list_and_navigation_logic() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/diagnostics.scm"))
            .unwrap();
        // next-error/previous-error are the unified dispatchers in shell.scm; with
        // no compile run, *next-error-source* stays "diagnostics" so they route here.
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        engine.eval(include_str!("../../scheme/shell.scm")).unwrap();
        // Inject diagnostics through the snapshot — the real primitive path.
        engine.set_snapshot(EditorSnapshot {
            line: 0,
            diagnostics: vec![
                "5\t2\terror\tunbound x".into(),
                "10\t0\twarning\tunused y".into(),
            ],
            ..Default::default()
        });
        // Parsing: leading number is the 0-based source line.
        assert_eq!(
            engine
                .eval(r#"(diag-line-of "5\t2\terror\tx")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("5")
        );
        // next-error from line 0 jumps to the nearest below (5).
        assert!(matches!(
            engine.eval("(next-error)").unwrap().commands.as_slice(),
            [EditorCommand::GotoLine(5)]
        ));
        // previous-error from line 0 has nothing above → a message, no jump.
        assert!(matches!(
            engine.eval("(previous-error)").unwrap().commands.as_slice(),
            [EditorCommand::Message(_)]
        ));
        // The list buffer renders one row per diagnostic (header + blank + 2).
        match engine
            .eval("(list-diagnostics)")
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::RenderBuffer {
                    name,
                    text,
                    quiet: false,
                    ..
                },
            ] => {
                assert_eq!(name, "*diagnostics*");
                assert!(
                    text.contains("6:3  error: unbound x"),
                    "1-based display, got:\n{text}"
                );
                assert!(text.contains("11:1  warning: unused y"));
            }
            other => panic!("expected RenderBuffer, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod compile_lib {
    use super::*;

    fn engine() -> (ScriptEngine, UnboundedReceiver<ScriptOutcome>) {
        let (engine, rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/diagnostics.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        engine.eval(include_str!("../../scheme/shell.scm")).unwrap();
        (engine, rx)
    }

    /// compile-run resolves its cwd via core.scm's `project-directory` (not
    /// `buffer-directory`), so it follows the selected project. Here we override
    /// `project-directory` before loading shell.scm so compile-run captures the
    /// override, proving the call site changed; the hook→perspective wiring is
    /// covered in perspectives_lib and by magit_and_project_scope_hooks_installed.
    #[test]
    fn compile_runs_in_project_directory() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/diagnostics.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        // Override must precede shell.scm: compile-run resolves `project-directory`
        // at definition time (Steel free-identifier resolution).
        engine
            .eval(r#"(define (project-directory) "/w/proj")"#)
            .unwrap();
        engine.eval(include_str!("../../scheme/shell.scm")).unwrap();
        let result = engine.eval(r#"(compile-run "make")"#).unwrap();
        let cd_in_proj = result.commands.iter().any(|c| match c {
            EditorCommand::MakeProcess(p) => {
                p.args.iter().any(|a| a.contains("cd '/w/proj' && make"))
            }
            _ => false,
        });
        assert!(
            cd_in_proj,
            "compile should cd into the project dir: {:?}",
            result.commands
        );
        assert_eq!(
            engine.eval("*compile-dir*").unwrap().value.as_deref(),
            Some("\"/w/proj\"")
        );
    }

    #[test]
    fn second_compile_prefills_last_command() {
        let (engine, _rx) = engine();
        // First run records the command.
        let result = engine.eval(r#"(compile-run "make test")"#).unwrap();
        assert!(
            result
                .commands
                .iter()
                .any(|c| matches!(c, EditorCommand::MakeProcess { .. }))
        );
        assert_eq!(
            engine.eval("*compile-last*").unwrap().value.as_deref(),
            Some("\"make test\"")
        );
        // Invoking `compile` again should open the minibuffer pre-filled.
        match engine.eval("(compile)").unwrap().commands.as_slice() {
            [
                EditorCommand::ReadString {
                    initial, callback, ..
                },
            ] => {
                assert_eq!(
                    initial, "make test",
                    "second compile should pre-fill last cmd"
                );
                assert_eq!(callback, "compile-run");
            }
            other => panic!("expected ReadString, got {other:?}"),
        }
    }

    /// The error parser recognizes the common compiler/grep formats and falls
    /// through to #f on plain text.
    #[test]
    fn compile_match_formats() {
        let (engine, _rx) = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // rust/cargo `  --> path:line:col`
        assert_eq!(
            v(r#"(nth (compile-match "  --> src/main.rs:10:5") 0)"#),
            "\"src/main.rs\""
        );
        assert_eq!(
            v(r#"(nth (compile-match "  --> src/main.rs:10:5") 1)"#),
            "10"
        );
        // python traceback (no column → 1)
        assert_eq!(
            v(r#"(nth (compile-match "  File \"foo.py\", line 42, in main") 0)"#),
            "\"foo.py\""
        );
        assert_eq!(
            v(r#"(nth (compile-match "  File \"foo.py\", line 42, in main") 2)"#),
            "1"
        );
        // gcc/clang `path:line:col:`
        assert_eq!(
            v(r#"(nth (compile-match "src/x.c:3:9: error: oops") 1)"#),
            "3"
        );
        // grep -n `path:line:` (no column)
        assert_eq!(
            v(r#"(nth (compile-match "README.md:7:matched text") 1)"#),
            "7"
        );
        assert_eq!(
            v(r#"(nth (compile-match "README.md:7:matched text") 2)"#),
            "1"
        );
        // plain prose → no match
        assert_eq!(
            v(r#"(if (compile-match "Compiling macros v0.1.0") "hit" "miss")"#),
            "\"miss\""
        );
    }

    /// Streaming records each error against its *compilation* line, and the final
    /// re-render faces those lines plus the status footer at the right index.
    #[test]
    fn streaming_and_faced_render() {
        let (engine, _rx) = engine();
        engine.eval(r#"(set! *compile-last* "make")"#).unwrap();
        engine.eval(r#"(set! *compile-dir* "/proj")"#).unwrap();
        engine.eval("(set! *compile-out* '())").unwrap();
        engine.eval("(set! *compile-errors* '())").unwrap();
        engine.eval("(set! *compile-line* 2)").unwrap();
        // Feed three output lines; the middle one is an error at *compilation* line 3.
        engine.eval(r#"(compile-output "Compiling")"#).unwrap();
        engine
            .eval(r#"(compile-output "src/x.c:3:9: error: oops")"#)
            .unwrap();
        engine.eval(r#"(compile-output "more output")"#).unwrap();
        // One error recorded, on comp-line 3 (header is lines 0-1, output starts at 2).
        assert_eq!(
            engine
                .eval("(length *compile-errors*)")
                .unwrap()
                .value
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            engine
                .eval("(car (car *compile-errors*))")
                .unwrap()
                .value
                .as_deref(),
            Some("3")
        );
        // Finishing re-renders with the error line + status line faced.
        match engine
            .eval(r#"(compile-done "2")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::RenderBuffer {
                    name,
                    text,
                    line_faces,
                    quiet: false,
                    ..
                },
                EditorCommand::Message(m),
            ] => {
                assert_eq!(name, "*compilation*");
                // The header names the cwd (Emacs's "Entering directory").
                assert!(
                    text.starts_with("$ cd /proj && make\n\n"),
                    "header preserved, got:\n{text}"
                );
                assert!(text.contains("Compilation exited abnormally with code 2"));
                // error line (3) is red; status line (last) is also red on failure.
                let last = text.split('\n').count() - 1;
                assert!(
                    line_faces.contains(&(3, "compilation-error".into())),
                    "faces: {line_faces:?}"
                );
                assert!(
                    line_faces.contains(&(last, "compilation-error".into())),
                    "faces: {line_faces:?}"
                );
                assert!(m.contains("1 error"), "summary: {m}");
            }
            other => panic!("expected RenderBuffer + Message, got {other:?}"),
        }
    }

    /// compile-run claims the next-error source, so the unified next-error walks
    /// the compilation errors (jumping to the source in another window).
    #[test]
    fn next_error_routes_to_compile() {
        let (engine, _rx) = engine();
        engine.eval(r#"(set! *compile-dir* "/proj")"#).unwrap();
        engine.eval("(set! *compile-out* '())").unwrap();
        engine.eval("(set! *compile-errors* '())").unwrap();
        engine.eval("(set! *compile-line* 2)").unwrap();
        engine.eval("(set! *compile-idx* -1)").unwrap();
        engine
            .eval(r#"(set! *next-error-source* "compile")"#)
            .unwrap();
        // Two errors; the file must exist for the jump (use this very source file).
        let here = concat!(env!("CARGO_MANIFEST_DIR"), "/src/script/mod.rs");
        engine
            .eval(&format!(r#"(compile-output "{here}:5:1: error: a")"#))
            .unwrap();
        engine
            .eval(&format!(r#"(compile-output "{here}:9:1: error: b")"#))
            .unwrap();
        // First next-error opens the file in another window and (once it is
        // current — open-file is async) jumps to line 5 (4 0-based).
        let pending_line = |engine: &ScriptEngine| {
            engine
                .eval("(list-ref *visit-pending* 2)")
                .unwrap()
                .value
                .unwrap()
        };
        let cmds = engine.eval("(next-error)").unwrap();
        assert!(
            cmds.commands.iter().any(|c| matches!(
                c,
                EditorCommand::RunWithTimer { callback, .. } if callback == "visit-location-step"
            )),
            "expected a deferred visit, got {:?}",
            cmds.commands
        );
        assert_eq!(pending_line(&engine), "4");
        assert!(
            cmds.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::OpenFileOtherWindow(_)))
        );
        // Second advances to the next error (line 9 → 8 0-based).
        engine.eval("(next-error)").unwrap();
        assert_eq!(pending_line(&engine), "8");
    }

    #[test]
    fn projectile_grep_goto_opens_file_and_jumps_line() {
        // projectile.scm depends on the rest of the bundled library, so load it all.
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        // grep-files emits 1-based `path:line:text`; selecting a match must open
        // the file and, once it has loaded (open-file is async), land on the
        // 0-based line 11. The jump is deferred, not queued behind the open —
        // a GotoLine in the same batch would move point in the OLD buffer.
        let cmds = engine
            .eval(r#"(projectile-grep-goto "/proj/src/main.rs:12:    let x = 1;")"#)
            .unwrap();
        assert!(
            cmds.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::OpenFile(p) if p == "/proj/src/main.rs")),
            "expected OpenFile(/proj/src/main.rs), got {:?}",
            cmds.commands
        );
        assert!(
            !cmds
                .commands
                .iter()
                .any(|c| matches!(c, EditorCommand::GotoLine(_))),
            "goto must wait for the file to load, got {:?}",
            cmds.commands
        );
        let pending = engine.eval("*visit-pending*").unwrap();
        assert_eq!(
            format!("{:?}", pending.value),
            format!(
                "{:?}",
                engine
                    .eval(r#"(list "" "/proj/src/main.rs" 11 0 0)"#)
                    .unwrap()
                    .value
            )
        );
    }
}

#[cfg(test)]
mod revert_cmd {
    use super::*;
    #[test]
    fn revert_buffer_is_a_builtin_command() {
        let (engine, _rx) = ScriptEngine::new();
        // Nullary builtin → in command-list → M-x discoverable.
        assert_eq!(
            engine
                .eval(r#"(if (member "revert-buffer" (command-list)) "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"y\"")
        );
        // And it enqueues the RevertBuffer command.
        assert!(matches!(
            engine.eval("(revert-buffer)").unwrap().commands.as_slice(),
            [EditorCommand::RevertBuffer]
        ));
    }

    #[test]
    fn macros_steel_mode_is_a_builtin_command() {
        let (engine, _rx) = ScriptEngine::new();
        // Nullary builtin → in command-list → M-x discoverable, so you can switch
        // any .scm buffer into the editor-aware mode by hand.
        assert_eq!(
            engine
                .eval(r#"(if (member "macros-steel-mode" (command-list)) "y" "n")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("\"y\"")
        );
        assert!(matches!(
            engine
                .eval("(macros-steel-mode)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::EnterMacrosSteel]
        ));
    }
}

#[cfg(test)]
mod display_modes {
    use super::*;
    #[test]
    fn line_number_and_scroll_bar_modes() {
        let (engine, _rx) = ScriptEngine::new();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        assert_eq!(
            v(r#"(if (member "line-numbers-mode" (command-list)) "y" "n")"#),
            "\"y\""
        );
        assert_eq!(
            v(r#"(if (member "scroll-bar-mode" (command-list)) "y" "n")"#),
            "\"y\""
        );
        assert_eq!(
            v(r#"(if (member "word-wrap-mode" (command-list)) "y" "n")"#),
            "\"y\""
        );
        assert!(matches!(
            engine
                .eval("(line-numbers-mode)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ToggleLineNumbers]
        ));
        assert!(matches!(
            engine
                .eval("(scroll-bar-mode)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ToggleScrollBar]
        ));
        assert!(matches!(
            engine.eval("(word-wrap-mode)").unwrap().commands.as_slice(),
            [EditorCommand::ToggleWordWrap]
        ));
        assert_eq!(
            v(r#"(if (member "electric-pair-mode" (command-list)) "y" "n")"#),
            "\"y\""
        );
        assert!(matches!(
            engine
                .eval("(electric-pair-mode)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ToggleElectricPair]
        ));
    }
}

#[cfg(test)]
mod window_targeting {
    use super::*;
    #[test]
    fn window_reads_and_commands() {
        let (engine, _rx) = ScriptEngine::new();
        engine.set_snapshot(EditorSnapshot {
            windows: vec![0, 2, 5],
            selected_window: 2,
            ..Default::default()
        });
        // read bridges over the snapshot
        assert_eq!(
            engine.eval("(selected-window)").unwrap().value.as_deref(),
            Some("2")
        );
        assert_eq!(
            engine.eval("(car (window-list))").unwrap().value.as_deref(),
            Some("0")
        );
        assert_eq!(
            engine
                .eval("(length (window-list))")
                .unwrap()
                .value
                .as_deref(),
            Some("3")
        );
        // command enqueues
        assert!(matches!(
            engine
                .eval("(select-window 5)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::SelectWindow(5)]
        ));
        assert!(
            matches!(engine.eval(r#"(open-file-other-window "/tmp/x.rs")"#).unwrap().commands.as_slice(),
            [EditorCommand::OpenFileOtherWindow(p)] if p == "/tmp/x.rs")
        );
    }

    #[test]
    fn get_buffer_window_finds_pane_or_minus_one() {
        let (engine, _rx) = ScriptEngine::new();
        engine.set_snapshot(EditorSnapshot {
            window_buffers: vec![(2, "main.rs".into()), (5, "*compilation*".into())],
            ..Default::default()
        });
        // A shown buffer reports its pane id; an unshown one reports -1.
        assert_eq!(
            engine
                .eval(r#"(get-buffer-window "*compilation*")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("5")
        );
        assert_eq!(
            engine
                .eval(r#"(get-buffer-window "main.rs")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("2")
        );
        assert_eq!(
            engine
                .eval(r#"(get-buffer-window "*scratch*")"#)
                .unwrap()
                .value
                .as_deref(),
            Some("-1")
        );
    }
}

#[cfg(test)]
mod icons {
    use super::*;
    #[test]
    fn assets_load_and_set_line_icon() {
        // The AssetSource resolves a bundled Lucide icon.
        use gpui::AssetSource;
        let bytes = crate::assets::Assets.load("icons/folder.svg").unwrap();
        assert!(bytes.is_some(), "folder.svg should be bundled");
        assert!(
            crate::assets::Assets
                .load("icons/file-code.svg")
                .unwrap()
                .is_some()
        );
        // `add-line-icon` appends with an explicit tint face.
        let (engine2, _rx2) = ScriptEngine::new();
        assert!(matches!(
            engine2.eval(r#"(add-line-icon 1 2 "chevron-down" "comment")"#).unwrap().commands.as_slice(),
            [EditorCommand::AddLineIcon(1, 2, n, f)] if n == "chevron-down" && f == "comment"
        ));
        // The primitive enqueues a SetLineIcon with the parsed args.
        let (engine, _rx) = ScriptEngine::new();
        assert!(matches!(
            engine.eval(r#"(set-line-icon 3 4 "folder")"#).unwrap().commands.as_slice(),
            [EditorCommand::SetLineIcon(3, 4, n)] if n == "folder"
        ));
        assert!(matches!(
            engine
                .eval("(clear-line-icons)")
                .unwrap()
                .commands
                .as_slice(),
            [EditorCommand::ClearLineIcons]
        ));
    }
}

#[cfg(test)]
mod fs_watch_lib {
    use super::*;

    /// The watch primitives and the sidebar auto-refresh path: `watch-directories`
    /// and `render-buffer-quiet` map to their commands, and the dired-sidebar/dired
    /// FS hooks render *quietly* (no focus-stealing switch) while re-declaring the
    /// directory set they want watched.
    #[test]
    fn watch_primitives_and_quiet_refresh() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // The hook handlers exist and are wired to `directory-changed-hook`.
        assert_eq!(
            v("(if (procedure? dired-sidebar-on-fs-change) \"y\" \"n\")"),
            "\"y\""
        );
        assert_eq!(
            v("(if (procedure? dired-on-fs-change) \"y\" \"n\")"),
            "\"y\""
        );

        // `(watch-directories owner dirs)` carries the owner + the full dir list.
        let cmds = engine
            .eval(r#"(watch-directories "dired-sidebar" (list "/a" "/b"))"#)
            .unwrap()
            .commands;
        match cmds.as_slice() {
            [EditorCommand::WatchDirectories(owner, dirs)] => {
                assert_eq!(owner, "dired-sidebar");
                assert_eq!(dirs, &vec!["/a".to_string(), "/b".to_string()]);
            }
            other => panic!("expected one WatchDirectories, got {other:?}"),
        }

        // `watch-tree` maps to the recursive variant.
        let cmds = engine
            .eval(r#"(watch-tree "magit" (list "/repo"))"#)
            .unwrap()
            .commands;
        assert!(
            matches!(
                cmds.as_slice(),
                [EditorCommand::WatchTree(owner, dirs)] if owner == "magit" && dirs == &vec!["/repo".to_string()]
            ),
            "watch-tree → WatchTree command"
        );

        // `render-buffer-quiet` is its own command (never the focus-stealing one).
        let cmds = engine
            .eval(r#"(render-buffer-quiet "*x*" "m" "hi" '())"#)
            .unwrap()
            .commands;
        assert!(matches!(
            cmds.as_slice(),
            [EditorCommand::RenderBuffer { name, quiet: true, .. }] if name == "*x*"
        ));

        // A dired-sidebar auto-refresh renders quietly and re-declares its watch
        // set (root + expanded dirs) for the "dired-sidebar" owner.
        let cmds = engine
            .eval(
                r#"(begin
                     (set! *dired-sidebar-root* ".")
                     (set! *dired-sidebar-expanded* '())
                     (set! *dired-sidebar-quiet* #t)
                     (dired-sidebar-render))"#,
            )
            .unwrap()
            .commands;
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::RenderBuffer { name, quiet: true, .. } if name == "*dired-sidebar*")),
            "quiet dired-sidebar refresh must use render-buffer-quiet"
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::WatchDirectories(owner, dirs) if owner == "dired-sidebar" && dirs == &vec![".".to_string()]
            )),
            "dired-sidebar render must re-declare its watched dirs"
        );

        // The interactive (non-quiet) path still uses the focus-switching render.
        let cmds = engine
            .eval(r#"(begin (set! *dired-sidebar-quiet* #f) (dired-sidebar-render))"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::RenderBuffer { name, .. } if name == "*dired-sidebar*")
            ),
            "interactive dired-sidebar refresh keeps the switching render-buffer"
        );
    }
}

#[cfg(test)]
mod dired_subtree_lib {
    use super::*;
    #[test]
    fn loads_and_maps_icons() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        assert_eq!(v(r#"(dired-subtree-icon "main.rs")"#), "\"file-code\"");
        assert_eq!(v(r#"(dired-subtree-icon "Cargo.toml")"#), "\"settings-2\"");
        assert_eq!(v(r#"(dired-subtree-icon "data.json")"#), "\"braces\"");
        assert_eq!(v(r#"(dired-subtree-icon "logo.png")"#), "\"image\"");
        assert_eq!(v(r#"(dired-subtree-icon "README.md")"#), "\"file-text\"");
        assert_eq!(v(r#"(dired-subtree-icon "notes")"#), "\"file\"");
        assert_eq!(v(r#"(strip-trailing-slash "/a/b/")"#), "\"/a/b\"");
        // Reveal: ancestors of a file from just inside the root down to its parent,
        // ordered root-first so they expand top-down.
        assert_eq!(
            v(r#"(dired-subtree-ancestors "/a" "/a/b/c/f.rs")"#),
            "(\"/a/b\" \"/a/b/c\")"
        );
        // A file directly in the root has no intermediate dirs to open.
        assert_eq!(v(r#"(dired-subtree-ancestors "/a" "/a/f.rs")"#), "()");
        // A file outside the root reveals nothing.
        assert_eq!(v(r#"(dired-subtree-ancestors "/a" "/x/y/f.rs")"#), "()");
        // A sibling whose name merely prefixes the root must not count as inside it.
        assert_eq!(v(r#"(dired-subtree-ancestors "/a" "/ab/f.rs")"#), "()");
        // dired-sidebar is the bundled file-tree command.
        assert_eq!(
            v(r#"(if (member "dired-sidebar" *interactive-commands*) "y" "n")"#),
            "\"y\""
        );
        // Indent rails: two columns ("│ ") per depth below the top level.
        assert_eq!(v(r"(dired-subtree-rails 0)"), "\"\"");
        assert_eq!(v(r"(dired-subtree-rails 2)"), "\"│ │ \"");
        // Whole-line faces reuse theme faces so colors track the active theme:
        // dotfiles and gitignored paths dim to comment; everything else —
        // directories included — keeps the default foreground (#f).
        assert_eq!(v(r#"(dired-subtree-face ".git" #f)"#), "\"comment\"");
        assert_eq!(v(r#"(dired-subtree-face "target" #t)"#), "\"comment\"");
        assert_eq!(v(r#"(if (dired-subtree-face "src" #f) "y" "n")"#), "\"n\"");
        // The crate's own target/ is gitignored, src/ is not.
        assert_eq!(
            v(
                r#"(let ((ig (dired-subtree-ignored "." (list "./target/" "./src/"))))
                   (list (hash-contains? ig "./target") (hash-contains? ig "./src")))"#
            ),
            "(#true #false)"
        );
        // Outside any listed paths git isn't consulted at all.
        assert_eq!(v(r#"(hash-length (dired-subtree-ignored "." '()))"#), "0");

        // The pure builder lists a real directory (the crate root): more than one
        // row (root + children), and the entry map resolves the root at line 0.
        assert_eq!(
            v(r#"(let ((b (dired-subtree-build "." '() '())))
                   (if (> (length (nth b 0)) 1) "y" "n"))"#),
            "\"y\""
        );
        assert_eq!(
            v(r#"(let ((b (dired-subtree-build "." '() '())))
                   (if (dired-subtree-entry-at (nth b 1) 0) "y" "n"))"#),
            "\"y\""
        );
        // The root's own row is omitted: the first row is a top-level child
        // (dirs sort first, so the crate's first directory), with no mark
        // gutter and no rails — just the 5 reserved chevron+icon columns.
        assert_eq!(
            v(r#"(let* ((b (dired-subtree-build "." '() '()))
                        (row (car (nth b 0))))
                   (list (starts-with? row "     ")
                         (starts-with? row "      ")
                         (car (cdr (dired-subtree-entry-at (nth b 1) 0)))))"#),
            "(#true #false #true)"
        );
        // A top-level dir row carries a chevron at byte 0 and an accent folder
        // icon after it, with no line face (the name keeps the foreground);
        // gitignored target/ dims its whole line, and its icon follows the line.
        // `row` → (icons-without-line-number line-face-or-#f) for PATH's row.
        let row = |path: &str| {
            v(&format!(
                r#"(let* ((b (dired-subtree-build "." '() '()))
                          (line (car (car (filter (lambda (e) (equal? (nth e 1) "{path}"))
                                                  (nth b 1)))))
                          (f (assoc line (nth b 3))))
                     (list (map cdr (filter (lambda (ic) (= (car ic) line)) (nth b 2)))
                           (if f (nth f 1) #f)))"#
            ))
        };
        assert_eq!(
            row("./src/"),
            r#"(((0 "chevron-right" "comment") (2 "folder" "keyword")) #false)"#
        );
        assert_eq!(
            row("./target/"),
            r#"(((0 "chevron-right" "comment") (2 "folder" "")) "comment")"#
        );
        assert_eq!(row("./Cargo.toml"), r#"(((2 "settings-2" "")) #false)"#);
        // Marking a row's path (taken from the entry map) re-renders that row
        // with the "D " gutter and gives every other row a blank one.
        assert_eq!(
            v(r#"(let* ((b (dired-subtree-build "." '() '()))
                        (p (car (dired-subtree-entry-at (nth b 1) 1)))
                        (m (dired-subtree-build "." '() (list p))))
                   (list (starts-with? (nth (nth m 0) 1) "D ")
                         (starts-with? (nth (nth m 0) 0) "       ")))"#),
            "(#true #true)"
        );
    }

    /// The evil/helix overlays give the sidebar top/bottom motions (`g g`,
    /// `G`, helix `g e`) WITHOUT touching its Emacs-default keymap: `g`
    /// becomes a prefix only under a modal mode, refresh moves to `g r` there,
    /// and with modal off the base single-`g` refresh is untouched (M-< / M->
    /// fall through to fundamental, so they need no binding here).
    #[test]
    fn sidebar_modal_overlay_motions() {
        let (engine, _rx) = ScriptEngine::new();
        let mut cmds = Vec::new();
        for src in crate::runtime::script_with_deps("dired-sidebar") {
            cmds.extend(engine.eval(src).unwrap().commands);
        }
        let bound = |mode: &str, key: &str, cmd: &str| {
            cmds.iter().any(|c| {
                matches!(c, EditorCommand::DefineKey(m, s, x)
                if m == mode && s == key && x == cmd)
            })
        };
        for prefix in ["evil-normal:", "helix-normal:"] {
            let km = format!("{prefix}dired-sidebar-mode");
            assert!(bound(&km, "g g", "evil-goto-first-line"));
            assert!(bound(&km, "shift-g", "evil-goto-last-line"));
            assert!(bound(&km, "g r", "dired-sidebar-refresh"));
        }
        assert!(bound(
            "helix-normal:dired-sidebar-mode",
            "g e",
            "evil-goto-last-line"
        ));
        assert!(bound("dired-sidebar-mode", "g", "dired-sidebar-refresh"));
    }
}

#[cfg(test)]
mod dired_file_ops {
    use super::*;

    fn engine() -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        engine
    }

    /// A fresh scratch directory (removed first if a previous run left it).
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("macros-dired-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Typed targets resolve like the file prompt: relative names join the
    /// directory, absolute / `~/` / `//` paths shadow it; an existing directory
    /// target means "into it"; marks follow a renamed entry.
    #[test]
    fn targets_resolve_and_marks_follow_renames() {
        let engine = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(v(r#"(dired-resolve "/a/b" "c.txt")"#), "\"/a/b/c.txt\"");
        assert_eq!(v(r#"(dired-resolve "/a/b/" "/tmp/x")"#), "\"/tmp/x\"");
        assert_eq!(
            v(r#"(dired-resolve "/a/b" "~/x")"#),
            format!("\"{home}/x\"")
        );
        let dir = scratch("dest");
        let d = dir.to_string_lossy();
        assert_eq!(
            v(&format!(r#"(dired-destination "/src/f.txt" "{d}")"#)),
            format!("\"{d}/f.txt\"")
        );
        assert_eq!(
            v(&format!(
                r#"(dired-destination "/src/f.txt" "{d}/new.txt")"#
            )),
            format!("\"{d}/new.txt\"")
        );
        assert_eq!(
            v(r#"(dired-retarget "/a/b.txt" "/a/b.txt" "/a/c.txt")"#),
            "\"/a/c.txt\""
        );
        assert_eq!(
            v(r#"(dired-retarget "/a/sub/" "/a/sub" "/a/new")"#),
            "\"/a/new/\""
        );
        assert_eq!(
            v(r#"(dired-retarget "/a/sub/f" "/a/sub/" "/a/new")"#),
            "\"/a/new/f\""
        );
        assert_eq!(
            v(r#"(dired-retarget "/a/subway" "/a/sub" "/a/new")"#),
            "\"/a/subway\""
        );
        engine
            .eval(r#"(begin (set! *dired-dir* "/nonexistent-dired/") (set! *dired-marks* (list "/nonexistent-dired/a.txt" "/nonexistent-dired/b")) (dired-moved "/nonexistent-dired/a.txt" "/nonexistent-dired/z.txt"))"#)
            .unwrap();
        assert_eq!(
            v("*dired-marks*"),
            r#"("/nonexistent-dired/z.txt" "/nonexistent-dired/b")"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rename moves the file and re-points its buffers; a failing one echoes
    /// the tool's error instead of claiming success; a clash asks first.
    #[test]
    fn moves_report_failures_and_confirm_overwrites() {
        let engine = engine();
        let dir = scratch("move");
        let d = dir.to_string_lossy().into_owned();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();

        let out = engine
            .eval(&format!(
                r#"(dired-move-file "{d}/a.txt" "{d}" "c.txt" (lambda (dest) dest))"#
            ))
            .unwrap();
        assert!(!dir.join("a.txt").exists() && dir.join("c.txt").exists());
        assert!(out.commands.iter().any(|c| matches!(
            c,
            EditorCommand::RetargetFileBuffers(old, new)
                if *old == format!("{d}/a.txt") && *new == format!("{d}/c.txt")
        )));

        // Into a directory that doesn't exist: mv fails, and says so.
        let out = engine
            .eval(&format!(
                r#"(dired-move-file "{d}/c.txt" "{d}" "nope/x.txt" (lambda (dest) dest))"#
            ))
            .unwrap();
        assert!(dir.join("c.txt").exists());
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RetargetFileBuffers(..)))
        );
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::Message(m) if m.starts_with("Renamed")))
        );
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::Message(m) if !m.is_empty()))
        );

        // Onto an existing file: nothing moves until the user confirms.
        let out = engine
            .eval(&format!(
                r#"(dired-move-file "{d}/c.txt" "{d}" "b.txt" (lambda (dest) dest))"#
            ))
            .unwrap();
        assert!(dir.join("c.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b");
        assert!(out.commands.iter().any(
            |c| matches!(c, EditorCommand::YesOrNo { prompt, .. } if prompt.contains("b.txt"))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `^` at the filesystem root stays at "/" instead of listing "".
    #[test]
    fn dired_up_stops_at_root() {
        let engine = engine();
        engine.eval(r#"(begin (dired-in "/") (dired-up))"#).unwrap();
        assert_eq!(engine.eval("*dired-dir*").unwrap().value.unwrap(), "\"/\"");
    }

    /// Sidebar: collapsing a directory drops the marks it hides, and a rename
    /// carries marks and open subdirectories to the new name.
    #[test]
    fn sidebar_marks_follow_collapse_and_rename() {
        let engine = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(
                r#"(begin
                     (set! *dired-sidebar-root* "/nonexistent-sb")
                     (set! *dired-sidebar-expanded* (list "/nonexistent-sb/sub" "/nonexistent-sb/sub/deep"))
                     (set! *dired-sidebar-marks* (list "/nonexistent-sb/sub/a.txt" "/nonexistent-sb/b.txt"))
                     (dired-sidebar-moved "/nonexistent-sb/sub" "/nonexistent-sb/new"))"#,
            )
            .unwrap();
        assert_eq!(
            v("*dired-sidebar-expanded*"),
            r#"("/nonexistent-sb/new" "/nonexistent-sb/new/deep")"#
        );
        assert_eq!(
            v("*dired-sidebar-marks*"),
            r#"("/nonexistent-sb/new/a.txt" "/nonexistent-sb/b.txt")"#
        );
        // Point (line 0) on the open "new/" directory: RET collapses it.
        engine
            .eval(
                r#"(begin
                     (set! *dired-sidebar-entries* (list (list 0 "/nonexistent-sb/new/" #t)))
                     (dired-sidebar-visit))"#,
            )
            .unwrap();
        assert_eq!(v("*dired-sidebar-marks*"), r#"("/nonexistent-sb/b.txt")"#);
        assert_eq!(
            v("*dired-sidebar-expanded*"),
            r#"("/nonexistent-sb/new/deep")"#
        );
    }

    /// A symlinked directory in the tree must not abort `git check-ignore`
    /// (git refuses paths "beyond a symbolic link"), which used to un-dim
    /// every ignored entry.
    #[cfg(unix)]
    #[test]
    fn gitignore_dimming_survives_symlinked_dirs() {
        let engine = engine();
        let dir = scratch("ignore");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git here
        }
        std::fs::write(dir.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        std::fs::create_dir_all(dir.join("real/deep")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let d = dir.to_string_lossy();
        let q = format!(
            r#"(let ((ig (dired-subtree-ignored "{d}" (list "{d}/ignored/" "{d}/link/" "{d}/link/deep/" "{d}/real/"))))
                 (list (hash-contains? ig "{d}/ignored") (hash-contains? ig "{d}/real")))"#
        );
        assert_eq!(engine.eval(&q).unwrap().value.unwrap(), "(#true #false)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod section_lib {
    use super::*;

    /// Build a two-level section tree and assert the rendered text, the
    /// point→section lookup, and the fold-toggle/default-collapsed logic.
    #[test]
    fn sections_render_navigate_and_fold() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        engine
            .eval(
                r#"(begin
                     (section-begin)
                     (section-line "Head" "h")
                     (section-open 'files "Files (2)" "h")
                     (section-line "  a.txt")
                     (section-line "  b.txt")
                     (section-close 'files))"#,
            )
            .unwrap();

        // Rendering emits a read-only buffer with the accumulated text (followed
        // by the overlay/fold commands that section-apply-overlays queues).
        let cmds = engine
            .eval(r#"(section-render "*sec*" "sec-mode")"#)
            .unwrap()
            .commands;
        let render = cmds
            .iter()
            .find_map(|c| match c {
                EditorCommand::RenderBuffer {
                    name, mode, text, ..
                } => Some((name, mode, text)),
                _ => None,
            })
            .expect("a RenderBuffer command");
        assert_eq!(render.0, "*sec*");
        assert_eq!(render.1, "sec-mode");
        assert_eq!(render.2, "Head\nFiles (2)\n  a.txt\n  b.txt");

        // Heading is line 1; its body covers lines 2..4. Lookups resolve to the id.
        assert_eq!(v("(section-id (section-at-line 1))"), "files");
        assert_eq!(v("(section-id (section-at-line 3))"), "files");
        assert_eq!(v("(if (section-at-line 0) \"hit\" \"miss\")"), "\"miss\""); // the Head line

        // Default-expanded: nothing folded until toggled.
        assert_eq!(v("(section-folded? 'files)"), "#false");
        engine
            .eval("(set! *section-toggled* (list 'files))")
            .unwrap();
        assert_eq!(v("(section-folded? 'files)"), "#true");

        // With default-collapsed, the meaning of a toggle flips.
        engine.eval("(section-default-collapsed! #t)").unwrap();
        engine.eval("(set! *section-toggled* '())").unwrap();
        assert_eq!(v("(section-folded? 'files)"), "#true");
    }
}

#[cfg(test)]
mod tabulated_lib {
    use super::*;

    /// A tabulated render pads columns and emits a header face; sorting reorders
    /// the body rows.
    #[test]
    fn tabulated_formats_and_sorts() {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();

        // Column padding: "ab" in a width-4 column, then a flexible last column.
        assert_eq!(
            v(r#"(tabulated-format (list "ab" "c") (list 4 #f))"#),
            "\"ab   c\""
        );

        let render = engine
            .eval(
                r#"(tabulated-render "*tab*" "tab-mode"
                     (list (list "Name" 6) (list "V" #f))
                     (list (list 'b (list "bbb" "2")) (list 'a (list "aaa" "1")))
                     (lambda (id) (message "x")))"#,
            )
            .unwrap();
        match render.commands.as_slice() {
            [
                EditorCommand::RenderBuffer {
                    text, line_faces, ..
                },
            ] => {
                // Header line + unsorted rows (b before a), columns aligned.
                assert_eq!(text, "Name   V\nbbb    2\naaa    1");
                assert_eq!(line_faces, &[(0, "tabulated-header".to_string())]);
            }
            other => panic!("expected one RenderBuffer, got {other:?}"),
        }

        // The sort helper orders rows by a column's cell (ascending), and the
        // descending flag reverses it. (The interactive `tabulated-sort` command
        // drives this off the active buffer, which a headless engine lacks.)
        assert_eq!(
            v(r#"(map (lambda (r) (symbol->string (car r)))
                   (tabulated-sort-rows
                     (list (list 'b (list "bbb" "2")) (list 'a (list "aaa" "1"))) 0 #f))"#),
            "(\"a\" \"b\")"
        );
        assert_eq!(
            v(r#"(map (lambda (r) (symbol->string (car r)))
                   (tabulated-sort-rows
                     (list (list 'b (list "bbb" "2")) (list 'a (list "aaa" "1"))) 0 #t))"#),
            "(\"b\" \"a\")"
        );
    }
}

#[cfg(test)]
mod capf_lib {
    use super::*;

    /// The bundled dabbrev CAPF is registered, tokenizes buffer words, and
    /// declines on too-short prefixes.
    #[test]
    fn dabbrev_capf_helpers() {
        let (engine, _rx) = ScriptEngine::new();
        let mut registered = false;
        for src in crate::runtime::startup_scripts() {
            let out = engine.eval(src).unwrap();
            registered |= out
                .commands
                .iter()
                .any(|c| matches!(c, EditorCommand::AddCapf(f) if f == "dabbrev-capf"));
        }
        assert!(registered, "completion.scm should register dabbrev-capf");

        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // Word tokenizer splits on non-word chars, keeping _ and -.
        assert_eq!(
            v(r#"(capf-buffer-words "foo bar, foo_baz qux-1")"#),
            "(\"foo\" \"bar\" \"foo_baz\" \"qux-1\")"
        );
        // Dedup preserves first-seen order.
        assert_eq!(
            v(r#"(dedup (list "a" "b" "a" "c" "b"))"#),
            "(\"a\" \"b\" \"c\")"
        );
        // An empty prefix is declined; a single character is enough to offer.
        assert_eq!(v(r#"(dabbrev-capf "")"#), "()");
    }
}

#[cfg(test)]
mod perspectives_lib {
    use super::*;

    /// core + helm + a stubbed persistence-dir (a unique per-test sandbox, so
    /// perspective-load starts empty and nothing touches the real config —
    /// see `fresh_persistence_dir`), then perspectives.scm itself — the same
    /// sorted-order prefix the runtime uses.
    fn engine() -> (ScriptEngine, UnboundedReceiver<ScriptOutcome>) {
        let (engine, rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/helm.scm")).unwrap();
        engine
            .eval(&format!(
                r#"(define (persistence-dir) "{}")"#,
                fresh_persistence_dir()
            ))
            .unwrap();
        // magit.scm declares these in production (it loads first); perspectives.scm
        // installs the hook via the setter so magit follows the title-bar project.
        engine
            .eval(
                r#"(define *magit-scope-hook* #f)
                   (define (set-magit-scope-hook! f) (set! *magit-scope-hook* f))"#,
            )
            .unwrap();
        engine
            .eval(include_str!("../../scheme/titlebar-menu.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/perspectives.scm"))
            .unwrap();
        (engine, rx)
    }

    /// First touch creates a perspective, a second touch of the same root only
    /// updates its resume file (order and length preserved).
    #[test]
    fn perspective_update_creates_then_refreshes() {
        let (engine, _rx) = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(r#"(perspective-update "/w/proj" "/w/proj/a.rs")"#)
            .unwrap();
        engine
            .eval(r#"(perspective-update "/w/other" "/w/other/b.rs")"#)
            .unwrap();
        engine
            .eval(r#"(perspective-update "/w/proj" "/w/proj/c.rs")"#)
            .unwrap();
        assert_eq!(v("(length *perspectives*)"), "2");
        assert_eq!(v("(car (car *perspectives*))"), "\"/w/proj\"");
        assert_eq!(
            v(r#"(alist-ref *perspectives* "/w/proj")"#),
            "\"/w/proj/c.rs\""
        );
    }

    /// Labels are bare basenames until two roots collide, then both qualify
    /// with their parent directory; the reverse lookup honors the same rule.
    #[test]
    fn perspective_display_disambiguates_duplicates() {
        let (engine, _rx) = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(r#"(perspective-update "/a/b/proj" "/a/b/proj/x.rs")"#)
            .unwrap();
        assert_eq!(v(r#"(perspective-display "/a/b/proj")"#), "\"proj\"");
        engine
            .eval(r#"(perspective-update "/z/other/proj" "/z/other/proj/y.rs")"#)
            .unwrap();
        assert_eq!(v(r#"(perspective-display "/a/b/proj")"#), "\"b/proj\"");
        assert_eq!(
            v(r#"(perspective-display "/z/other/proj")"#),
            "\"other/proj\""
        );
        assert_eq!(
            v(r#"(perspective-root-for-name "b/proj")"#),
            "\"/a/b/proj\""
        );
        assert_eq!(
            v(r#"(if (perspective-root-for-name "nope") "hit" "miss")"#),
            "\"miss\""
        );
    }

    /// One qualification level isn't always enough: /x/a/proj and /y/a/proj
    /// would both label "a/proj", the checkmark would mark both rows, and the
    /// label→root reverse lookup would open whichever registered first. Labels
    /// extend segment by segment until unique.
    #[test]
    fn perspective_display_qualifies_until_unique() {
        let (engine, _rx) = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(r#"(perspective-update "/x/a/proj" "/x/a/proj/1.rs")"#)
            .unwrap();
        engine
            .eval(r#"(perspective-update "/y/a/proj" "/y/a/proj/2.rs")"#)
            .unwrap();
        assert_eq!(v(r#"(perspective-display "/x/a/proj")"#), "\"x/a/proj\"");
        assert_eq!(v(r#"(perspective-display "/y/a/proj")"#), "\"y/a/proj\"");
        assert_eq!(
            v(r#"(perspective-root-for-name "y/a/proj")"#),
            "\"/y/a/proj\""
        );
    }

    /// Syncing pushes the full label list, the current root's label, and the
    /// `perspective-select` callback to the title bar.
    #[test]
    fn perspective_sync_pushes_titlebar_dropdown() {
        let (engine, _rx) = engine();
        engine
            .eval(r#"(perspective-update "/w/proj" "/w/proj/a.rs")"#)
            .unwrap();
        engine
            .eval(r#"(perspective-update "/w/other" "/w/other/b.rs")"#)
            .unwrap();
        engine
            .eval(r#"(set! *perspective-current* "/w/other")"#)
            .unwrap();
        match engine
            .eval("(perspective-sync-titlebar)")
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::SetTitlebarMenu(m)] => {
                assert_eq!(m.title, "Projects");
                assert_eq!(m.items, ["proj", "other"]);
                assert_eq!(m.commands, ["", ""]);
                assert_eq!(m.active, "other");
                assert_eq!(m.callback, "perspective-select");
            }
            other => panic!("expected SetTitlebarMenu, got {other:?}"),
        }
    }

    /// Selecting a perspective whose resume file vanished (but whose root
    /// still exists) falls back to a find-file Helm picker in that root
    /// instead of opening a dead path (after marking the picked perspective
    /// current in the title bar).
    #[test]
    fn perspective_select_missing_file_falls_back_to_helm() {
        let (engine, _rx) = engine();
        let root = std::env::temp_dir().join("macros-persp-test-missing-file");
        std::fs::create_dir_all(&root).unwrap();
        engine
            .eval(&format!(
                r#"(perspective-update "{0}" "{0}/gone.rs")"#,
                root.display()
            ))
            .unwrap();
        match engine
            .eval(r#"(perspective-select "macros-persp-test-missing-file")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::SetTitlebarMenu(m),
                EditorCommand::Helm(sources),
            ] => {
                assert_eq!(m.active, "macros-persp-test-missing-file");
                assert_eq!(sources.len(), 1);
            }
            other => panic!("expected SetTitlebarMenu + Helm fallback, got {other:?}"),
        }
    }

    /// Selecting a perspective whose ROOT vanished (project deleted or
    /// unmounted mid-session — startup only prunes at load) drops it from the
    /// list and the title bar instead of switching to a dead project over an
    /// empty picker.
    #[test]
    fn perspective_select_vanished_root_removes_perspective() {
        let (engine, _rx) = engine();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(r#"(perspective-update "/nonexistent-macros-test/gone" "/nonexistent-macros-test/gone/a.rs")"#)
            .unwrap();
        engine
            .eval(r#"(set! *perspective-current* "/nonexistent-macros-test/gone")"#)
            .unwrap();
        let outcome = engine.eval(r#"(perspective-select "gone")"#).unwrap();
        assert_eq!(v("(length *perspectives*)"), "0");
        assert_eq!(
            v(r#"(if *perspective-current* "set" "unset")"#),
            "\"unset\""
        );
        // The title bar was re-published without the dead perspective (an
        // empty item list clears the dropdown entirely).
        assert!(
            outcome.commands.iter().any(|c| matches!(
                c,
                EditorCommand::SetTitlebarMenu(m) if m.items.is_empty()
            )),
            "expected an empty SetTitlebarMenu, got {:?}",
            outcome.commands
        );
    }

    /// The current perspective is sticky: entering a fileless buffer (dired,
    /// the sidebar, terminal, magit, *scratch*) or a file outside every
    /// project leaves the title bar on the project being worked in. Only
    /// visiting a file inside some project moves it. Regression: it used to
    /// clear to the neutral trigger on every sidebar or terminal visit.
    #[test]
    fn perspective_track_sticks_across_non_project_buffers() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/helm.scm")).unwrap();
        // Stubs must precede perspectives.scm: Steel resolves free identifiers
        // at definition time, so later redefinitions wouldn't be seen.
        engine
            .eval(&format!(
                r#"
                (define (persistence-dir) "{}")
                (define *stub-root* "/w/proj")
                (define *stub-file* "/w/proj/a.rs")
                (define (project-root d) *stub-root*)
                (define (buffer-directory) "/w/proj")
                (define (buffer-file-name) *stub-file*)
                (define *magit-scope-hook* #f)
                (define (set-magit-scope-hook! f) (set! *magit-scope-hook* f))
                "#,
                fresh_persistence_dir()
            ))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/titlebar-menu.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/perspectives.scm"))
            .unwrap();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // Visiting a project file makes its perspective current.
        engine.eval("(perspective-track)").unwrap();
        assert_eq!(v("*perspective-current*"), "\"/w/proj\"");
        // A fileless buffer (dired / sidebar / terminal) leaves it alone.
        engine.eval(r#"(set! *stub-file* "")"#).unwrap();
        engine.eval("(perspective-track)").unwrap();
        assert_eq!(v("*perspective-current*"), "\"/w/proj\"");
        // So does a file outside every project root.
        engine
            .eval(r#"(begin (set! *stub-root* #f) (set! *stub-file* "/elsewhere/x.txt"))"#)
            .unwrap();
        engine.eval("(perspective-track)").unwrap();
        assert_eq!(v("*perspective-current*"), "\"/w/proj\"");
    }

    /// perspectives.scm installs both scope hooks so project commands follow the
    /// title bar: *magit-scope-hook* (magit's git work-tree) and *project-dir-hook*
    /// (compile/term's project root). Each resolves the CURRENT perspective, and
    /// #f when none is active (so callers fall back to the current buffer).
    #[test]
    fn perspective_installs_scope_hooks() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/helm.scm")).unwrap();
        // Stubs precede perspectives.scm so its hooks capture them (definition-
        // time resolution). gix-toplevel echoes its arg so the resolved root
        // flows through; the buffer/project stubs let perspective-track set one.
        // *magit-scope-hook* is stubbed because magit.scm isn't loaded here;
        // *project-dir-hook* comes from core.scm (loaded above).
        engine
            .eval(&format!(
                r#"
                (define (persistence-dir) "{}")
                (define (project-root d) "/w/proj")
                (define (buffer-directory) "/w/proj")
                (define (buffer-file-name) "/w/proj/a.rs")
                (define (gix-toplevel d) d)
                (define *magit-scope-hook* #f)
                (define (set-magit-scope-hook! f) (set! *magit-scope-hook* f))
                "#,
                fresh_persistence_dir()
            ))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/titlebar-menu.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/perspectives.scm"))
            .unwrap();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        // Both installed real thunks over the defaults.
        assert_eq!(
            v("(if (procedure? *magit-scope-hook*) \"y\" \"n\")"),
            "\"y\""
        );
        assert_eq!(
            v("(if (procedure? *project-dir-hook*) \"y\" \"n\")"),
            "\"y\""
        );
        // No perspective active yet → both #f (callers fall back to the buffer).
        assert_eq!(v(r#"(if (*magit-scope-hook*) "y" "n")"#), "\"n\"");
        assert_eq!(v(r#"(if (*project-dir-hook*) "y" "n")"#), "\"n\"");
        // After visiting a project file, both resolve that project's root.
        engine.eval("(perspective-track)").unwrap();
        assert_eq!(v("(*magit-scope-hook*)"), "\"/w/proj\"");
        assert_eq!(v("(*project-dir-hook*)"), "\"/w/proj\"");
    }

    /// Switching back to a perspective whose resume file is already open gets
    /// no find-file-hook (`open-file` only fires it on a fresh visit), so
    /// perspective-select must push the title-bar update itself. Regression:
    /// the bar kept showing the previous project when toggling back and forth.
    #[test]
    fn perspective_select_syncs_titlebar_before_opening() {
        let (engine, _rx) = engine();
        let root = std::env::temp_dir().join("macros-persp-test");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("resume.rs");
        std::fs::write(&file, "").unwrap();
        engine
            .eval(&format!(
                r#"(perspective-update "{}" "{}")"#,
                root.display(),
                file.display()
            ))
            .unwrap();
        engine
            .eval(r#"(perspective-update "/nonexistent-macros-test/other" "/nonexistent-macros-test/other/x.rs")"#)
            .unwrap();
        engine
            .eval(r#"(set! *perspective-current* "/nonexistent-macros-test/other")"#)
            .unwrap();
        match engine
            .eval(r#"(perspective-select "macros-persp-test")"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::SetTitlebarMenu(m),
                EditorCommand::OpenFile(path),
            ] => {
                assert_eq!(m.active, "macros-persp-test");
                assert!(path.ends_with("resume.rs"), "path: {path}");
            }
            other => panic!("expected SetTitlebarMenu + OpenFile, got {other:?}"),
        }
    }
}

/// Regressions for the modal-keybinding fixes: Helix vertical motion must carry
/// a CHARACTER column (a byte column lands inside a multibyte char), the vim
/// word motions must be class-aware, and the help commands must follow the
/// active modal keymap instead of assuming `fundamental`.
#[cfg(test)]
mod modal_keys {
    use super::*;

    /// A ScriptEngine with every startup script loaded. Returns the outcome
    /// receiver too: the caller binds it so the channel stays open for the
    /// engine's lifetime.
    fn engine_with_startup() -> (
        ScriptEngine,
        futures::channel::mpsc::UnboundedReceiver<crate::script::ScriptOutcome>,
    ) {
        let (engine, rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        (engine, rx)
    }

    /// Helix `j`/`k` preserve the character column, so a move onto a line with
    /// multibyte text lands ON a character — never inside one. With the old
    /// byte-column arithmetic, moving down from column 2 of "abcd" onto "aébc"
    /// produced byte 7, the second byte of `é`, which the rope refuses to slice
    /// at.
    #[test]
    fn helix_vertical_motion_lands_on_char_boundaries() {
        let (engine, _rx) = engine_with_startup();
        // line 0 "abcd" = bytes 0..4; line 1 "aébc" starts at byte 5, where
        // 'a'=5, 'é'=6..8, 'b'=8, 'c'=9. Byte 7 is mid-codepoint.
        let text = "abcd\naébc\n";
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str(text),
            selections: vec![(2, 2)],
            ..Default::default()
        });
        let pos = match engine
            .eval("(helix-line-down)")
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::SetSelections(p)] => p[0].1,
            other => panic!("expected one SetSelections, got {other:?}"),
        };
        assert!(
            text.is_char_boundary(pos),
            "helix-line-down produced mid-codepoint byte {pos} in {text:?}"
        );
        // Column 2 of "aébc" is the 'b', at byte 8.
        assert_eq!(pos, 8);

        // And `k` at the top line stays put rather than clamping to line 0 and
        // silently resetting the column.
        engine.set_snapshot(EditorSnapshot {
            text: Rope::from_str(text),
            selections: vec![(2, 2)],
            ..Default::default()
        });
        match engine.eval("(helix-line-up)").unwrap().commands.as_slice() {
            [EditorCommand::SetSelections(p)] => assert_eq!(p[0], (2, 2)),
            other => panic!("expected one SetSelections, got {other:?}"),
        }
    }

    /// Vim `b` / `e` are class-aware — the mirrors of `w` — so they stop inside
    /// a punctuation run. Emacs's `backward-word` / `forward-word` skip
    /// punctuation as a separator and so disagree with `w` about where words
    /// are; that asymmetry is what these motions replace.
    #[test]
    fn vim_word_motions_are_class_aware() {
        use crate::buffer::Buffer;
        let b = Buffer::from_rope(Rope::from_str("foo.bar baz"));
        // `b` from the start of "bar" (byte 4) stops on the '.', not on "foo".
        assert_eq!(b.backward_word_start(4), 3);
        assert_eq!(b.backward_word_start(3), 0);
        // `e` from 0 lands ON the last char of "foo" (byte 2), then on the '.'.
        assert_eq!(b.forward_word_end(0), 2);
        assert_eq!(b.forward_word_end(2), 3);
        // Emacs `backward-word` is unchanged and still skips the punctuation.
        assert_eq!(b.backward_word(4), 0);
        // WORD motions treat "foo.bar" as one token.
        assert_eq!(b.forward_bigword_start(0), 8);
        assert_eq!(b.forward_bigword_end(0), 6);
        assert_eq!(b.backward_bigword_start(8), 0);
    }

    /// Character columns round-trip through the vertical-motion bridges, and a
    /// column past a short line clamps to that line's end (still on a boundary).
    #[test]
    fn char_column_bridges_round_trip() {
        use crate::buffer::Buffer;
        let b = Buffer::from_rope(Rope::from_str("aébc\nxy\n"));
        assert_eq!(b.char_column(3), 2); // byte 3 = 'b' = char column 2
        assert_eq!(b.byte_at_char_column(0, 2), 3);
        // "aébc" is bytes 0..5 (é is two bytes), so line 1 "xy" is bytes 6..8.
        // A column past its end clamps to that end — still a char boundary.
        assert_eq!(b.byte_at_char_column(1, 9), 8);
        assert_eq!(b.byte_at_char_column(1, 1), 7);
    }

    /// The help commands follow the ACTIVE modal keymap. Under evil, `j` must
    /// resolve (it is bound in `evil-normal`); with modal editing off, the same
    /// lookup falls back to `fundamental` only.
    #[test]
    fn help_follows_the_active_modal_keymap() {
        let (engine, _rx) = engine_with_startup();
        let keymaps = |evil: bool, helix: bool| {
            engine.set_snapshot(EditorSnapshot {
                evil_enabled: evil,
                helix_enabled: helix,
                ..Default::default()
            });
            engine
                .eval("(active-keymaps)")
                .unwrap()
                .value
                .unwrap_or_default()
        };
        assert!(keymaps(false, false).contains("fundamental"));
        assert!(!keymaps(false, false).contains("evil-normal"));
        assert!(keymaps(true, false).contains("evil-normal"));
        // Helix wins even though `evil-enabled?` is also true there (one shared
        // state machine, two keymap families).
        let h = keymaps(true, true);
        assert!(h.contains("helix-normal"), "got {h}");
        assert!(!h.contains("evil-normal"), "got {h}");
    }

    /// Every keymap chord is bound exactly once across all startup scripts.
    /// `define-key` is a map insert, so a second binding for the same chord
    /// silently wins by file load order and the first command becomes
    /// unreachable by key — which is how `C-c c`, `C-c C-d`, `C-c C-f` and
    /// `C-h b` each ended up with a dead binding.
    #[test]
    fn no_chord_is_silently_double_bound() {
        use std::collections::HashMap;
        // Deliberate overrides: a later package upgrades an earlier command on
        // the same chord (helm/ibuffer replacing the plain builtins, undo-tree
        // re-stating evil's C-r with the identical command).
        const ALLOWED: &[(&str, &str)] = &[
            ("fundamental", "alt-x"),
            ("fundamental", "ctrl-x b"),
            ("fundamental", "ctrl-x ctrl-b"),
            ("fundamental", "ctrl-x ctrl-f"),
            ("evil-normal", "ctrl-r"),
        ];
        let re = regex::Regex::new(r#"\(define-key\s+"([^"]+)"\s+"([^"]+)""#).unwrap();
        let mut seen: HashMap<(String, String), usize> = HashMap::new();
        for src in crate::runtime::startup_scripts() {
            for c in re.captures_iter(src) {
                *seen
                    .entry((c[1].to_string(), c[2].to_string()))
                    .or_default() += 1;
            }
        }
        let mut dupes: Vec<String> = seen
            .into_iter()
            .filter(|((km, chord), n)| *n > 1 && !ALLOWED.contains(&(km.as_str(), chord.as_str())))
            .map(|((km, chord), n)| format!("{km} {chord:?} bound {n}x"))
            .collect();
        dupes.sort();
        assert!(
            dupes.is_empty(),
            "double-bound chords:\n  {}",
            dupes.join("\n  ")
        );
    }
}

#[cfg(test)]
mod dap_lib {
    use super::*;

    fn engine_with_libs() -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).expect("a startup script failed to load");
        }
        engine
    }

    /// Every Scheme-side state file resolves into the per-engine test sandbox,
    /// never the developer's real ~/.config/macros (regression: the DAP tests
    /// below once left breakpoints in the real dap-breakpoints file).
    #[test]
    fn state_files_resolve_outside_real_config() {
        let engine = engine_with_libs();
        let s = |q: &str| {
            engine
                .eval(q)
                .unwrap()
                .value
                .unwrap()
                .trim_matches('"')
                .to_string()
        };
        let sandbox = crate::runtime::persistence_directory();
        let real = format!(
            "{}/.config/macros",
            std::env::var("HOME").unwrap_or_default()
        );
        for q in [
            "(persistence-dir)",
            "(dap--bp-file)",
            "(saveplace-file)",
            "(recentf-file)",
        ] {
            let path = s(q);
            assert!(
                !path.starts_with(&real),
                "{q} → {path} is the real config dir"
            );
            assert!(
                path.starts_with(std::env::temp_dir().to_str().unwrap()),
                "{q} → {path} not sandboxed"
            );
            // Per-engine: not the test thread's own sandbox.
            assert!(
                !path.starts_with(sandbox.to_str().unwrap()),
                "{q} shares a sandbox"
            );
        }
    }

    /// dap-request stamps sequential `seq`s and dap--on-message routes a
    /// response body back to the stored Scheme callback by `request_seq`.
    #[test]
    fn responses_correlate_to_request_callbacks() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(
                r#"(begin
                     (set! *dap-session* 99)   ; pretend a transport is up
                     (define dap-test-got "")
                     (dap-request "evaluate" (hash "expression" "x")
                                  (lambda (body) (set! dap-test-got (dap--str body 'result)))))"#,
            )
            .unwrap();
        // The response to the wrong seq is ignored; the right one fires.
        let _ = engine.eval(
            r#"(dap--on-message
                 "{\"type\":\"response\",\"request_seq\":42,\"success\":true,\"command\":\"evaluate\",\"body\":{\"result\":\"nope\"}}")"#,
        );
        assert_eq!(v("dap-test-got"), "\"\"");
        let _ = engine.eval(
            r#"(dap--on-message
                 "{\"type\":\"response\",\"request_seq\":1,\"success\":true,\"command\":\"evaluate\",\"body\":{\"result\":\"42\"}}")"#,
        );
        assert_eq!(v("dap-test-got"), "\"42\"");
        assert_eq!(
            v("(length *dap-pending*)"),
            "0",
            "pending entry not consumed"
        );
    }

    /// A `stopped` event flips the state machine, records the thread, and the
    /// stackTrace response populates the frames + selects the innermost.
    #[test]
    fn stopped_event_updates_state_thread_and_frames() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine.eval(r#"(set! *dap-session* 99)"#).unwrap();
        let _ = engine.eval(
            r#"(dap--on-message
                 "{\"type\":\"event\",\"event\":\"stopped\",\"body\":{\"reason\":\"breakpoint\",\"threadId\":7}}")"#,
        );
        assert_eq!(v("(eq? *dap-state* 'stopped)"), "#true");
        assert_eq!(v("*dap-thread*"), "7");
        // Answer the stackTrace request (the most recent seq). The frame's
        // source doesn't exist, so the jump is skipped but the selection isn't.
        let _ = engine.eval(
            r#"(dap--on-message
                 (string-append
                   "{\"type\":\"response\",\"request_seq\":" (int->string *dap-seq*)
                   ",\"success\":true,\"command\":\"stackTrace\",\"body\":{\"stackFrames\":"
                   "[{\"id\":3,\"name\":\"main\",\"line\":10,\"source\":{\"path\":\"/nonexistent/x.py\"}}]}}"))"#,
        );
        assert_eq!(v("(length *dap-frames*)"), "1");
        assert_eq!(v("*dap-frame-idx*"), "0");
        assert_eq!(v("(dap--current-frame-id)"), "3");
    }

    /// Breakpoint bookkeeping: per-file lookup, distinct files, condition
    /// replacement, and the unverified marking from a setBreakpoints response.
    #[test]
    fn breakpoint_bookkeeping_and_verification() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(
                r#"(set! *dap-breakpoints*
                     (list (list "/a.py" 3 "") (list "/a.py" 9 "n > 2") (list "/b.go" 5 "")))"#,
            )
            .unwrap();
        assert_eq!(v(r#"(length (dap--bps-for "/a.py"))"#), "2");
        assert_eq!(v(r#"(length (dap--breakpoint-files))"#), "2");
        assert_eq!(v(r#"(nth (dap--bp-at "/a.py" 9) 2)"#), "\"n > 2\"");
        assert_eq!(v(r#"(dap--bp-at "/a.py" 4)"#), "#false");
        // The adapter verifies the first /a.py breakpoint but not the second.
        engine
            .eval(
                r#"(dap--note-verified "/a.py"
                     (list (hash 'verified #t) (hash 'verified #f)))"#,
            )
            .unwrap();
        assert_eq!(
            v(r#"(if (member (cons "/a.py" 9) *dap-bp-unverified*) #t #f)"#),
            "#true"
        );
        assert_eq!(
            v(r#"(if (member (cons "/a.py" 3) *dap-bp-unverified*) #t #f)"#),
            "#false"
        );
        // Setting a condition on an existing line replaces, not duplicates.
        engine
            .eval(r#"(begin (set! *dap--cond-target* (list "/a.py" 3)) (dap--set-condition "x == 1"))"#)
            .unwrap();
        assert_eq!(v(r#"(length (dap--bps-for "/a.py"))"#), "2");
        assert_eq!(v(r#"(nth (dap--bp-at "/a.py" 3) 2)"#), "\"x == 1\"");
    }

    /// Adapter selection by file extension (debugpy for .py, delve for .go).
    #[test]
    fn adapter_registry_matches_extensions() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        assert_eq!(v(r#"(car (dap--adapter-for "/x/app.py"))"#), "\"debugpy\"");
        assert_eq!(v(r#"(car (dap--adapter-for "/x/main.go"))"#), "\"delve\"");
        assert_eq!(v(r#"(dap--adapter-for "/x/lib.rs")"#), "#false");
    }

    /// The panel build renders threads/stack/scopes/breakpoints from seeded
    /// state and records the RET navigation tables.
    #[test]
    fn panel_build_renders_sections_and_nav_tables() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval(
                r#"(begin
                     (set! *dap-state* 'stopped)
                     (set! *dap-stopped-reason* "breakpoint")
                     (set! *dap-adapter* (dap--adapter-for "/x/app.py"))
                     (set! *dap-thread* 1)
                     (set! *dap-threads* (list (hash 'id 1 'name "MainThread")))
                     (set! *dap-frames* (list (hash 'id 3 'name "fetch" 'line 15
                                                    'source (hash 'path "/x/app.py"))))
                     (set! *dap-frame-idx* 0)
                     (set! *dap-scopes* (list (list "Locals" 12 #f
                                                    (list (hash 'name "resp" 'value "Response"
                                                                'variablesReference 40)))))
                     (set! *dap-breakpoints* (list (list "/x/app.py" 14 "")))
                     (dap-ui--build))"#,
            )
            .unwrap();
        let text = v("(section-text)");
        assert!(text.contains("Debug: debugpy"), "header missing: {text}");
        assert!(
            text.contains("stopped: breakpoint"),
            "state missing: {text}"
        );
        assert!(text.contains("MainThread"), "threads missing: {text}");
        assert!(text.contains("fetch"), "stack missing: {text}");
        assert!(text.contains("resp = Response"), "locals missing: {text}");
        assert!(text.contains("app.py:14"), "breakpoints missing: {text}");
        // The expandable variable landed in the RET table with its varref.
        assert_eq!(v("(length *dap-ui--var-lines*)"), "1");
        assert_eq!(v("(cdr (car *dap-ui--var-lines*))"), "40");
        assert_eq!(v("(length *dap-ui--frame-lines*)"), "1");
    }

    /// delve listens on `:0`; the port comes from its announcement line.
    #[test]
    fn delve_port_parsed_from_announcement() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        assert_eq!(
            v(r#"(dap--delve-port "DAP server listening at: 127.0.0.1:53211")"#),
            "53211"
        );
        assert_eq!(
            v(r#"(dap--delve-port "Type 'dlv help' for list")"#),
            "#false"
        );
    }

    /// Output for a console that isn't on screen is held in a capped backlog
    /// instead of `append-to-buffer`, which would switch the selected window.
    #[test]
    fn hidden_console_output_is_backlogged_not_focus_stealing() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        let out = engine.eval(r#"(dap--console-append "hello\n")"#).unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::AppendToBuffer { .. })),
            "appended to a hidden console: {:?}",
            out.commands
        );
        assert_eq!(v("*dap--console-backlog*"), "\"hello\\n\"");
        engine
            .eval(r#"(dap--console-append (make-string 70000 #\a))"#)
            .unwrap();
        assert_eq!(v("(string-length *dap--console-backlog*)"), "65536");
    }

    /// A force-kill timer only reaps the session it was scheduled for, and
    /// dap-quit resets a session stuck "starting" with no transport.
    #[test]
    fn force_kill_is_pinned_and_quit_resets_starting() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        engine
            .eval("(begin (set! *dap-session* 99) (dap--schedule-kill) (set! *dap-session* 100))")
            .unwrap();
        let out = engine.eval("(dap--force-kill)").unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::DapKill { .. })),
            "killed the wrong session"
        );
        engine.eval("(set! *dap-session* 99)").unwrap();
        let out = engine.eval("(dap--force-kill)").unwrap();
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::DapKill { id: 99 })),
            "scheduled session not killed: {:?}",
            out.commands
        );
        engine
            .eval("(begin (set! *dap-session* -1) (set! *dap-state* 'starting) (dap-quit))")
            .unwrap();
        assert_eq!(v("(eq? *dap-state* 'dead)"), "#true");
    }

    /// Jumping to a file that isn't open parks the goto (open-file is async)
    /// for the find-file-hook instead of moving point in the current buffer.
    #[test]
    fn jump_to_unopened_file_is_deferred_to_find_file_hook() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        let out = engine
            .eval(r#"(dap--visit "/nonexistent-dap/x.py" 12 #t)"#)
            .unwrap();
        assert!(
            matches!(out.commands.as_slice(), [EditorCommand::OpenFile(p)] if p == "/nonexistent-dap/x.py"),
            "expected only OpenFile: {:?}",
            out.commands
        );
        assert_eq!(v("(nth *dap--pending-jump* 1)"), "12");
    }

    /// file-truename resolves symlinks (the adapter reports real paths) and
    /// passes a nonexistent path through unchanged.
    #[test]
    fn file_truename_resolves_symlinks() {
        let engine = engine_with_libs();
        let v = |q: &str| engine.eval(q).unwrap().value.unwrap();
        let dir = std::env::temp_dir().join(format!("macros-truename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.py");
        std::fs::write(&real, "").unwrap();
        let link = dir.join("link.py");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(unix)]
        assert_eq!(
            v(&format!("(file-truename {:?})", link.display().to_string())),
            format!(
                "{:?}",
                std::fs::canonicalize(&real).unwrap().display().to_string()
            )
        );
        assert_eq!(
            v(r#"(file-truename "/nonexistent-dap/y.py")"#),
            "\"/nonexistent-dap/y.py\""
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod session_lib {
    use super::*;

    /// core + hooks + stubs for what session.scm reads from earlier files
    /// (persistence-dir → a unique sandbox, the perspectives accessors), then
    /// session.scm itself.
    fn engine() -> (ScriptEngine, UnboundedReceiver<ScriptOutcome>, String) {
        let (engine, rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/hooks.scm")).unwrap();
        let dir = fresh_persistence_dir();
        engine
            .eval(&format!(
                r#"(define (persistence-dir) "{dir}")
                   (define *test-persp* #f)
                   (define (perspective-current) "/proj/a")
                   (define (perspective-restore-current! root) (set! *test-persp* root))"#
            ))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/session.scm"))
            .expect("session.scm loads");
        (engine, rx, dir)
    }

    fn v(engine: &ScriptEngine, q: &str) -> String {
        engine
            .eval(q)
            .unwrap_or_else(|e| panic!("{q}: {e}"))
            .value
            .unwrap_or_default()
    }

    #[test]
    fn escape_round_trips_newlines_tabs_and_backslashes() {
        let (engine, _rx, _) = engine();
        // Literal backslash-n (two chars), a real newline, a tab, a trailing
        // backslash, and a CR — all survive.
        let src = r#"(define s (string-append "a\\nb" "\n" "\tc\\" "\r" "d"))"#;
        engine.eval(src).unwrap();
        assert_eq!(
            v(&engine, "(equal? (session-unescape (session-escape s)) s)"),
            "#true"
        );
        // The escaped form is a single line with no raw tab.
        assert_eq!(
            v(&engine, r#"(string-contains? (session-escape s) "\n")"#),
            "#false"
        );
        assert_eq!(
            v(&engine, r#"(string-contains? (session-escape s) "\t")"#),
            "#false"
        );
    }

    #[test]
    fn savehist_serialize_parse_round_trip_and_bounds() {
        let (engine, _rx, _) = engine();
        engine
            .eval(
                r#"(define rings (list (cons "minibuffer" (list "a" "b\nc"))
                                       (cons "search" (list "foo"))
                                       (cons "command" (list "magit-status"))
                                       (cons "kill" (list "k1" "k\t2"))))"#,
            )
            .unwrap();
        assert_eq!(
            v(
                &engine,
                "(equal? (savehist-parse (savehist-serialize rings)) rings)"
            ),
            "#true"
        );
        // Per-ring cap keeps the NEWEST entries.
        engine.eval("(savehist-set-max 1)").unwrap();
        assert_eq!(
            v(
                &engine,
                r#"(alist-ref (savehist-parse (savehist-serialize rings)) "minibuffer")"#
            ),
            r#"("b\nc")"#
        );
        // Kill ring opt-out: nothing written for it.
        engine.eval("(savehist-save-kill-ring #f)").unwrap();
        assert_eq!(
            v(
                &engine,
                r#"(alist-ref (savehist-parse (savehist-serialize rings)) "kill")"#
            ),
            "()"
        );
    }

    #[test]
    fn savehist_save_then_load_restores_every_ring() {
        let (engine, _rx, dir) = engine();
        engine.set_session(SessionState {
            minibuffer_history: vec!["find me".into(), "two\nlines".into()],
            kill_ring: vec!["killed".into()],
            search_ring: vec!["needle".into()],
            command_history: vec!["magit-status".into()],
            ..Default::default()
        });
        engine.eval("(savehist-save)").unwrap();
        assert!(std::path::Path::new(&format!("{dir}/savehist")).exists());
        let out = engine.eval("(savehist-load)").unwrap();
        let mut rings: Vec<(String, Vec<String>)> = out
            .commands
            .iter()
            .filter_map(|c| match c {
                EditorCommand::SetSessionRing { ring, items } => {
                    Some((ring.clone(), items.clone()))
                }
                _ => None,
            })
            .collect();
        rings.sort();
        assert_eq!(
            rings,
            vec![
                ("command".to_string(), vec!["magit-status".to_string()]),
                ("kill".to_string(), vec!["killed".to_string()]),
                (
                    "minibuffer".to_string(),
                    vec!["find me".to_string(), "two\nlines".to_string()]
                ),
                ("search".to_string(), vec!["needle".to_string()]),
            ]
        );
    }

    #[test]
    fn layout_lines_round_trip_and_prune() {
        let (engine, _rx, _) = engine();
        engine
            .eval(
                r#"(define tree
                     (list 'split #f 0.3
                           (list 'leaf "main.rs" "/p/src/main.rs" 42 #t)
                           (list 'split #t 0.5
                                 (list 'leaf "*compilation*" "" 0 #f)
                                 (list 'leaf "lib.rs" "/p/src/lib.rs" 7 #f))))"#,
            )
            .unwrap();
        assert_eq!(
            v(
                &engine,
                "(equal? (desktop-lines->layout (desktop-layout->lines tree)) tree)"
            ),
            "#true"
        );
        // Dropping the fileless *compilation* leaf collapses its split.
        assert_eq!(
            v(
                &engine,
                r#"(desktop-prune-layout tree (lambda (l) (not (equal? (nth l 2) ""))))"#
            ),
            r#"(split #false 0.3 (leaf "main.rs" "/p/src/main.rs" 42 #true) (leaf "lib.rs" "/p/src/lib.rs" 7 #false))"#
        );
        assert_eq!(
            v(&engine, "(desktop-prune-layout tree (lambda (l) #f))"),
            "#false"
        );
        // Truncated input parses to #f, not half a tree.
        assert_eq!(
            v(
                &engine,
                r#"(desktop-lines->layout (list "split\th\t0.5" "leaf\t0\t1\ta\t/a"))"#
            ),
            "#false"
        );
    }

    /// The real path: the editor publishes a layout, desktop serializes it to
    /// disk text, parses it back, and `set-window-layout` queues exactly the
    /// tree that was published.
    #[test]
    fn desktop_file_round_trips_the_published_layout() {
        let (engine, _rx, _) = engine();
        let layout = LayoutSpec::Split {
            vertical: true,
            ratio: 0.25,
            a: Box::new(LayoutSpec::Leaf {
                buffer: "a.rs".into(),
                path: "/p/a.rs".into(),
                point: 3,
                selected: false,
            }),
            b: Box::new(LayoutSpec::Leaf {
                buffer: "b.rs".into(),
                path: "/p/b.rs".into(),
                point: 99,
                selected: true,
            }),
        };
        engine.set_session(SessionState {
            file_buffers: vec![("/p/a.rs".into(), 3), ("/p/b.rs".into(), 99)],
            layout: Some(layout.clone()),
            ..Default::default()
        });
        engine
            .eval(
                "(define parsed (desktop-parse (desktop-serialize (session-file-buffers) \
                 (window-layout) (perspective-current))))",
            )
            .unwrap();
        assert_eq!(
            v(&engine, "(car parsed)"),
            r#"(("/p/a.rs" 3) ("/p/b.rs" 99))"#
        );
        assert_eq!(v(&engine, "(nth parsed 2)"), r#""/proj/a""#);
        let out = engine.eval("(set-window-layout (cadr parsed))").unwrap();
        assert_eq!(out.value.as_deref(), Some("#true"));
        match out.commands.as_slice() {
            [EditorCommand::SetWindowLayout(spec)] => assert_eq!(spec, &layout),
            other => panic!("expected one SetWindowLayout, got {other:?}"),
        }
    }

    /// desktop-restore skips missing files, prunes their panes, and restores the
    /// perspective; a desktop whose panes all vanished still lands on a file.
    #[test]
    fn desktop_restore_skips_missing_files() {
        let (engine, _rx, dir) = engine();
        let real = format!("{dir}/real.txt");
        std::fs::write(&real, "hello").unwrap();
        let text = format!(
            "# macros desktop v1\nperspective\t{dir}\nbuffer\t2\t{real}\nbuffer\t0\t{dir}/gone.txt\n\
             split\th\t0.5\nleaf\t2\t1\treal.txt\t{real}\nleaf\t0\t0\tgone.txt\t{dir}/gone.txt"
        );
        engine
            .eval(&format!(
                "(define n (desktop-restore (desktop-parse {:?})))",
                text
            ))
            .map(|o| {
                let kinds: Vec<String> = o
                    .commands
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect();
                assert!(
                    kinds.iter().any(|k| k.starts_with("RestoreFileBuffers")
                        && k.contains("real.txt")
                        && !k.contains("gone.txt")),
                    "{kinds:?}"
                );
                assert!(
                    o.commands.iter().any(|c| matches!(
                        c,
                        EditorCommand::SetWindowLayout(LayoutSpec::Leaf { path, selected: true, .. })
                            if path == &real
                    )),
                    "the split collapses onto the surviving pane: {kinds:?}"
                );
            })
            .unwrap();
        assert_eq!(v(&engine, "n"), "1");
        assert_eq!(v(&engine, "*test-persp*"), format!("{dir:?}"));
    }

    #[test]
    fn desktop_clear_deletes_file_and_stops_autosave() {
        let (engine, _rx, dir) = engine();
        engine.eval("(set! *desktop-primary* #t)").unwrap();
        engine.set_session(SessionState::default());
        engine.eval("(session-save-all)").unwrap();
        let file = format!("{dir}/desktop");
        assert!(std::path::Path::new(&file).exists(), "autosave wrote it");
        engine.eval("(desktop-clear)").unwrap();
        assert!(!std::path::Path::new(&file).exists());
        engine.eval("(session-save-all)").unwrap();
        assert!(
            !std::path::Path::new(&file).exists(),
            "cleared desktop isn't rewritten by autosave"
        );
        // desktop-save re-arms (it goes through session-refresh first).
        let out = engine.eval("(desktop-save)").unwrap();
        assert!(
            out.commands.iter().any(
                |c| matches!(c, EditorCommand::SessionRefresh(cb) if cb == "desktop--save-now")
            )
        );
        assert_eq!(v(&engine, "(desktop-auto-save?)"), "#true");
    }

    /// The after-init entry point and the autosave tick run cleanly and keep
    /// the timer armed (by name — run-with-timer takes a string callback).
    #[test]
    fn startup_and_autosave_tick_arm_the_timer() {
        let (engine, _rx, _) = engine();
        let timer = |out: &EvalResult| {
            out.commands.iter().any(|c| {
                matches!(c, EditorCommand::RunWithTimer { ms: 300000, callback }
                    if callback == "session-autosave-tick")
            })
        };
        let out = engine.eval("(session-startup)").expect("startup runs");
        assert!(timer(&out), "{:?}", out.commands);
        let out = engine.eval("(session-autosave-tick)").expect("tick runs");
        assert!(timer(&out), "{:?}", out.commands);
        assert!(
            out.commands.iter().any(
                |c| matches!(c, EditorCommand::SessionRefresh(cb) if cb == "session-save-all")
            )
        );
        // Opting out from init.scm turns the whole thing off.
        engine
            .eval("(savehist-mode #f) (desktop-save-mode #f)")
            .unwrap();
        assert_eq!(v(&engine, "(desktop-auto-save?)"), "#false");
    }

    /// Launched on explicit files: the saved desktop is neither restored nor
    /// overwritten by the quit/autosave path.
    #[test]
    fn file_launch_leaves_the_saved_desktop_alone() {
        let (engine, _rx, dir) = engine();
        let file = format!("{dir}/desktop");
        std::fs::write(&file, "saved desktop").unwrap();
        engine
            .eval(r#"(session-set-launch-files! (list "notes.txt"))"#)
            .unwrap();
        let out = engine.eval("(session-startup)").unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RestoreFileBuffers(_))),
            "restored the desktop: {:?}",
            out.commands
        );
        assert_eq!(v(&engine, "(desktop-auto-save?)"), "#false");
        engine.set_session(SessionState::default());
        engine.eval("(session-save-all)").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "saved desktop");
    }

    #[test]
    fn session_hooks_are_registered() {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine.eval(include_str!("../../scheme/hooks.scm")).unwrap();
        engine
            .eval(
                r#"(define (persistence-dir) "/nonexistent-session-test")
                   (define (perspective-current) #f)
                   (define (perspective-restore-current! r) #f)"#,
            )
            .unwrap();
        let out = engine
            .eval(include_str!("../../scheme/session.scm"))
            .unwrap();
        let hooks: Vec<String> = out
            .commands
            .iter()
            .filter_map(|c| match c {
                EditorCommand::AddHook(hook, _) => Some(hook.clone()),
                _ => None,
            })
            .collect();
        assert!(hooks.contains(&"after-init-hook".to_string()), "{hooks:?}");
        assert!(hooks.contains(&"kill-emacs-hook".to_string()), "{hooks:?}");
    }
}

#[cfg(test)]
mod test_runner_lib {
    use super::*;

    fn engine() -> (ScriptEngine, UnboundedReceiver<ScriptOutcome>) {
        let (engine, rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/diagnostics.scm"))
            .unwrap();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        engine.eval(include_str!("../../scheme/shell.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/test-runner.scm"))
            .expect("test-runner.scm loads");
        (engine, rx)
    }

    fn v(engine: &ScriptEngine, q: &str) -> String {
        engine
            .eval(q)
            .unwrap_or_else(|e| panic!("{q}: {e}"))
            .value
            .unwrap_or_default()
    }

    /// Evaluate `(f text line)` for a source text given as a Rust string.
    fn at(engine: &ScriptEngine, f: &str, text: &str, line: usize) -> String {
        engine.eval(&format!("(define src {text:?})")).unwrap();
        v(engine, &format!("({f} src {line})"))
    }

    #[test]
    fn steel_files_use_the_steel_runner() {
        let (engine, _rx) = engine();
        assert_eq!(
            v(&engine, r#"(test-language-of "lib/util.scm")"#),
            r#""steel""#
        );
        assert_eq!(
            v(
                &engine,
                r#"(test-command 'file "steel" "tests/util.scm" "" 0)"#
            ),
            r#"("steel 'tests/util.scm'" . #false)"#
        );
        // No suite command: the steel CLI runs one file at a time.
        assert_eq!(
            v(&engine, r#"(car (test-command 'project "steel" "" "" 0))"#),
            "#false"
        );
        // `executable-find` answers "" for something that isn't installed.
        assert_eq!(
            v(
                &engine,
                r#"(executable-find "surely-not-a-real-program-xyz")"#
            ),
            r#""""#
        );
    }

    const RUST: &str = r#"use std::fmt;

fn helper() -> u32 { 1 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_numbers() {
        let s = "{ not a brace }";
        assert_eq!(helper(), 1); // }
    }

    mod nested {
        #[tokio::test]
        #[should_panic]
        async fn deep_one() {
            if true {
                panic!();
            }
        }
    }

    fn not_a_test() {
        let x = 1;
    }
}
"#;

    #[test]
    fn rust_enclosing_test_and_module_path() {
        let (engine, _rx) = engine();
        // Inside adds_numbers (brace in a string/comment doesn't confuse depth).
        assert_eq!(
            at(&engine, "test-rust-point", RUST, 11),
            r#"(("tests") "adds_numbers")"#
        );
        // On the fn line itself, and on its closing brace.
        assert_eq!(
            at(&engine, "test-rust-point", RUST, 9),
            r#"(("tests") "adds_numbers")"#
        );
        assert_eq!(
            at(&engine, "test-rust-point", RUST, 12),
            r#"(("tests") "adds_numbers")"#
        );
        // Nested module, attribute stack with #[should_panic] between.
        assert_eq!(
            at(&engine, "test-rust-point", RUST, 19),
            r#"(("tests" "nested") "deep_one")"#
        );
        // A non-test fn, the space between tests, and non-test code: none.
        assert_eq!(at(&engine, "test-rust-point", RUST, 25), "#false");
        assert_eq!(at(&engine, "test-rust-point", RUST, 13), "#false");
        assert_eq!(at(&engine, "test-rust-point", RUST, 2), "#false");
    }

    #[test]
    fn rust_file_targets() {
        let (engine, _rx) = engine();
        for (rel, want) in [
            ("src/script/tests.rs", r#"("" . "script::tests")"#),
            ("src/editor/mod.rs", r#"("" . "editor")"#),
            ("src/main.rs", r#"("" . "")"#),
            ("tests/integration.rs", r#"("--test integration " . "")"#),
            ("src/bin/tool.rs", r#"("--bin tool " . "")"#),
        ] {
            assert_eq!(
                v(&engine, &format!("(test-rust-file-target {rel:?})")),
                want,
                "{rel}"
            );
        }
        engine.eval(&format!("(define src {RUST:?})")).unwrap();
        assert_eq!(
            v(
                &engine,
                r#"(test-command 'point "rust" "src/calc.rs" src 19)"#
            ),
            r#"("cargo test calc::tests::nested::deep_one" . #false)"#
        );
        assert_eq!(
            v(
                &engine,
                r#"(test-command 'file "rust" "tests/it.rs" src 0)"#
            ),
            r#"("cargo test --test it " . #false)"#
        );
        assert_eq!(
            v(
                &engine,
                r#"(test-command 'file "rust" "src/calc.rs" src 0)"#
            ),
            r#"("cargo test calc::" . #false)"#
        );
        assert_eq!(
            v(
                &engine,
                r#"(cdr (test-command 'point "rust" "src/calc.rs" src 2))"#
            ),
            r#""Point is not inside a #[test] function""#
        );
    }

    const PY: &str = "import pytest\n\nclass TestMath:\n    def setup_method(self):\n        pass\n\n    def test_add(self):\n        x = 1\n\n        assert x == 1\n\n@pytest.mark.slow\ndef test_top():\n    def inner():\n        pass\n    inner()\n";

    #[test]
    fn python_pytest_node_ids() {
        let (engine, _rx) = engine();
        assert_eq!(
            at(&engine, "test-python-point", PY, 9),
            r#""TestMath::test_add""#
        );
        // A blank line inside the method still belongs to it.
        assert_eq!(
            at(&engine, "test-python-point", PY, 8),
            r#""TestMath::test_add""#
        );
        // In the class but not in a test → the class.
        assert_eq!(at(&engine, "test-python-point", PY, 4), r#""TestMath""#);
        // A nested helper inside a top-level test → the test.
        assert_eq!(at(&engine, "test-python-point", PY, 14), r#""test_top""#);
        assert_eq!(at(&engine, "test-python-point", PY, 0), "#false");
        engine.eval(&format!("(define src {PY:?})")).unwrap();
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'point "python" "tests/test_m.py" src 9))"#
            ),
            r#""python -m pytest 'tests/test_m.py::TestMath::test_add'""#
        );
    }

    const GO: &str = "package calc\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(1, 2) != 3 {\n\t\tt.Fatal(\"bad\")\n\t}\n}\n\nfunc helper() int { return 1 }\n\nfunc TestSub(t *testing.T) {}\n";

    #[test]
    fn go_test_names() {
        let (engine, _rx) = engine();
        assert_eq!(at(&engine, "test-go-point", GO, 6), r#""TestAdd""#);
        assert_eq!(at(&engine, "test-go-point", GO, 10), "#false");
        assert_eq!(at(&engine, "test-go-point", GO, 2), "#false");
        engine.eval(&format!("(define src {GO:?})")).unwrap();
        assert_eq!(
            v(&engine, "(test-go-file-names src)"),
            r#"("TestAdd" "TestSub")"#
        );
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'point "go" "calc_test.go" src 6))"#
            ),
            r#""go test -run '^TestAdd$' .""#
        );
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'file "go" "calc_test.go" src 6))"#
            ),
            r#""go test -run '^(TestAdd|TestSub)$' .""#
        );
    }

    const JS: &str = "import { sum } from './sum';\n\ndescribe('sum', () => {\n  it(\"adds (1+2)\", () => {\n    expect(sum(1, 2)).toBe(3);\n  });\n\n  test.only(`it's fine`, () => {});\n});\n";

    #[test]
    fn js_test_names_and_runner_detection() {
        let (engine, _rx) = engine();
        assert_eq!(at(&engine, "test-js-point", JS, 4), r#""adds (1+2)""#);
        assert_eq!(at(&engine, "test-js-point", JS, 7), r#""it's fine""#);
        assert_eq!(at(&engine, "test-js-point", JS, 2), r#""sum""#);
        assert_eq!(at(&engine, "test-js-point", JS, 0), "#false");
        assert_eq!(
            v(
                &engine,
                r#"(test-js-runner "{\"devDependencies\": {\"vitest\": \"1\"}}")"#
            ),
            r#""vitest""#
        );
        assert_eq!(
            v(
                &engine,
                r#"(test-js-runner "{\"scripts\": {\"test\": \"jest\"}}")"#
            ),
            r#""jest""#
        );
        assert_eq!(v(&engine, r#"(test-js-runner "{}")"#), r#""npm""#);
        engine.eval(&format!("(define src {JS:?})")).unwrap();
        // The -t pattern is regex-escaped and shell-quoted.
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'point "jest" "src/sum.test.ts" src 4))"#
            ),
            r#""npx jest 'src/sum.test.ts' -t 'adds \\(1\\+2\\)'""#
        );
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'point "vitest" "a.test.ts" src 7))"#
            ),
            r#""npx vitest run 'a.test.ts' -t 'it'\\''s fine'""#
        );
    }

    #[test]
    fn zig_and_custom_templates() {
        let (engine, _rx) = engine();
        let zig = "const std = @import(\"std\");\n\ntest \"adds\" {\n    try std.testing.expect(true);\n}\n";
        assert_eq!(at(&engine, "test-zig-point", zig, 3), r#""adds""#);
        engine
            .eval(r#"(test-runner-set-command "rust" 'point "cargo nextest run {filter}")"#)
            .unwrap();
        engine.eval(&format!("(define src {RUST:?})")).unwrap();
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'point "rust" "src/lib.rs" src 11))"#
            ),
            r#""cargo nextest run tests::adds_numbers""#
        );
        // The other kinds are untouched.
        assert_eq!(
            v(
                &engine,
                r#"(car (test-command 'project "rust" "src/lib.rs" src 11))"#
            ),
            r#""cargo test""#
        );
    }

    /// End to end: point in a Rust test in a real temp crate → `test-at-point`
    /// launches cargo in the crate root through compile-mode, and
    /// `test-rerun-last` repeats it there.
    #[test]
    fn test_at_point_runs_in_crate_root_and_reruns() {
        let (engine, _rx) = engine();
        let root = fresh_persistence_dir();
        std::fs::write(format!("{root}/Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(format!("{root}/src")).unwrap();
        let file = format!("{root}/src/calc.rs");
        std::fs::write(&file, RUST).unwrap();
        engine.set_snapshot(EditorSnapshot {
            path: file.clone(),
            directory: format!("{root}/src"),
            text: Rope::from_str(RUST),
            line: 11,
            ..Default::default()
        });
        let ran = |out: &EvalResult| {
            out.commands.iter().any(|c| match c {
                EditorCommand::MakeProcess(p) => p.args.iter().any(|a| {
                    a.contains(&format!(
                        "cd '{root}' && cargo test calc::tests::adds_numbers"
                    ))
                }),
                _ => false,
            })
        };
        let out = engine.eval("(test-at-point)").unwrap();
        assert!(ran(&out), "{:?}", out.commands);
        // Finish the run so the compile lane is free again, then rerun.
        engine.eval(r#"(compile-done "0")"#).unwrap();
        let out = engine.eval("(test-rerun-last)").unwrap();
        assert!(ran(&out), "{:?}", out.commands);
        // recompile (`g` in *compilation*) stays pinned to the crate root.
        assert_eq!(v(&engine, "*compile-fixed-dir*"), format!("{root:?}"));
    }
}

/// query-replace / replace-string (replace.scm), occur + editable results
/// (occur.scm), and the `buffer-search-all` / `regexp-search-lines` bridges.
#[cfg(test)]
mod search_replace_tests {
    use super::*;

    fn engine_with(text: &str, point: usize) -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        // The receiver is dropped: these tests only read eval results.
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        engine.set_snapshot(EditorSnapshot {
            name: "src.txt".into(),
            text: Rope::from_str(text),
            point,
            ..Default::default()
        });
        engine
    }

    fn value(engine: &ScriptEngine, q: &str) -> String {
        engine.eval(q).unwrap().value.unwrap_or_default()
    }

    fn commands(engine: &ScriptEngine, q: &str) -> Vec<EditorCommand> {
        engine.eval(q).unwrap().commands
    }

    fn replacements(cmds: &[EditorCommand]) -> Vec<(usize, usize, String)> {
        cmds.iter()
            .filter_map(|c| match c {
                EditorCommand::ReplaceRegion { start, end, text } => {
                    Some((*start, *end, text.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Apply ReplaceRegion commands (byte offsets) to `text`, in order — what
    /// the editor does — so tests can check the resulting buffer.
    fn apply(text: &str, reps: &[(usize, usize, String)]) -> String {
        let mut s = text.to_string();
        for (a, b, t) in reps {
            s.replace_range(*a..*b, t);
        }
        s
    }

    #[test]
    fn replacement_expansion_backrefs_and_case() {
        let e = engine_with("", 0);
        assert_eq!(
            value(
                &e,
                r#"(qr-expand-replacement "<\\1|\\2|\\&|\\\\|\\9>" (list "ab" "a" "b"))"#
            ),
            r#""<a|b|ab|\\|>""#
        );
        // Multibyte groups and literal text survive the char walk.
        assert_eq!(
            value(&e, r#"(qr-expand-replacement "é\\1ü\\n" (list "x" "ö"))"#),
            "\"éöü\\n\""
        );
        // Emacs case-replace: a lower-case replacement copies the match's case.
        assert_eq!(value(&e, r#"(qr-case-adjust "bar" "FOO")"#), r#""BAR""#);
        assert_eq!(value(&e, r#"(qr-case-adjust "bar" "Foo")"#), r#""Bar""#);
        assert_eq!(value(&e, r#"(qr-case-adjust "bar" "foo")"#), r#""bar""#);
        assert_eq!(
            value(&e, r#"(qr-case-adjust "émile" "ÉCOLE")"#),
            r#""ÉMILE""#
        );
        // …but not when the user typed an upper-case replacement or FROM.
        assert_eq!(
            value(&e, r#"(qr-replacement-for (list 0 3 "FOO") "Bar" #f #t)"#),
            r#""Bar""#
        );
        assert_eq!(
            value(&e, r#"(qr-replacement-for (list 0 3 "FOO") "bar" #f #f)"#),
            r#""bar""#
        );
        // Smart case + literal quoting.
        assert_eq!(value(&e, r#"(qr-pattern "a.b" #f #t)"#), r#""(?i)a\\.b""#);
        assert_eq!(
            value(&e, r#"(qr-pattern "A.b" #t (qr-fold? "A.b"))"#),
            r#""A.b""#
        );
    }

    #[test]
    fn buffer_search_all_reports_byte_offsets_and_groups() {
        let e = engine_with("héllo wörld\nfoo bar foo\n", 0);
        assert_eq!(
            value(&e, r#"(buffer-search-all "(f)(o)o" 0 100)"#),
            r#"((14 17 "foo" "f" "o") (22 25 "foo" "f" "o"))"#
        );
        // `^` is a real line anchor even when the window starts mid-line, and
        // matches must lie wholly inside the window.
        assert_eq!(value(&e, r#"(buffer-search-all "^foo" 15 100)"#), "()");
        assert_eq!(
            value(&e, r#"(buffer-search-all "foo" 0 24)"#),
            r#"((14 17 "foo"))"#
        );
        assert!(value(&e, r#"(buffer-search-all "(" 0 10)"#).contains("unclosed group"));
        assert_eq!(
            value(&e, r#"(regexp-search-lines "o" (list "wö o" "x"))"#),
            "(((4 5)) ())"
        );
    }

    #[test]
    fn query_replace_session_over_multibyte_text() {
        let text = "héllo Foo wörld\nfoo bar FOO — foo\n";
        let e = engine_with(text, 0);
        let start = commands(&e, r#"(qr-start "foo" "bar" #f 0 (point-max) #f #f #f)"#);
        assert!(
            start
                .iter()
                .any(|c| matches!(c, EditorCommand::UndoAmalgamateBegin))
        );
        assert!(start.iter().any(
            |c| matches!(c, EditorCommand::ReadKeyStart { callback, .. } if callback == "qr-key")
        ));
        assert!(
            start
                .iter()
                .any(|c| matches!(c, EditorCommand::SetRegion { start: 7, end: 10 }))
        );
        let mut reps = Vec::new();
        reps.extend(replacements(&commands(&e, r#"(qr-key "y")"#))); // Foo -> Bar
        reps.extend(replacements(&commands(&e, r#"(qr-key "n")"#))); // skip foo
        reps.extend(replacements(&commands(&e, r#"(qr-key "!")"#))); // FOO, foo
        assert_eq!(apply(text, &reps), "héllo Bar wörld\nfoo bar BAR — bar\n");
        // The session already ended at `!`: a stray key only disarms.
        let done = commands(&e, r#"(qr-key "q")"#);
        assert!(replacements(&done).is_empty(), "{done:?}");
        assert_eq!(value(&e, "*qr-count*"), "3");
    }

    #[test]
    fn query_replace_undo_back_and_finish() {
        let text = "a1 a2 a3";
        let e = engine_with(text, 0);
        commands(
            &e,
            r#"(qr-start "a([0-9])" "<\\1>" #t 0 (point-max) #f #f #f)"#,
        );
        let mut reps = Vec::new();
        reps.extend(replacements(&commands(&e, r#"(qr-key " ")"#)));
        reps.extend(replacements(&commands(&e, r#"(qr-key "y")"#)));
        assert_eq!(apply(text, &reps), "<1> <2> a3");
        reps.extend(replacements(&commands(&e, r#"(qr-key "u")"#))); // a2 back
        assert_eq!(apply(text, &reps), "<1> a2 a3");
        reps.extend(replacements(&commands(&e, r#"(qr-key "^")"#)));
        reps.extend(replacements(&commands(&e, r#"(qr-key "U")"#)));
        assert_eq!(apply(text, &reps), text);
        reps.extend(replacements(&commands(&e, r#"(qr-key ".")"#)));
        assert_eq!(apply(text, &reps), "<1> a2 a3");
        assert_eq!(value(&e, "*qr-active?*"), "#false");
    }

    #[test]
    fn query_replace_quits_on_escape_and_stray_keys() {
        let e = engine_with("x x", 0);
        commands(&e, r#"(qr-start "x" "y" #f 0 (point-max) #f #f #f)"#);
        let cmds = commands(&e, r#"(qr-key "escape")"#);
        assert!(cmds.iter().any(|c| matches!(c, EditorCommand::ReadKeyStop)));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::UndoAmalgamateEnd))
        );
        assert!(replacements(&cmds).is_empty());
        // A key after the session ended just disarms the capture.
        let cmds = commands(&e, r#"(qr-key "y")"#);
        assert!(replacements(&cmds).is_empty());
    }

    #[test]
    fn replace_string_from_point_and_in_region() {
        let text = "ö-ab ab ab";
        // From point (byte 6, the 2nd "ab" — after the 2-byte ö) to the end.
        let e = engine_with(text, 6);
        let cmds = commands(
            &e,
            r#"(qr-replace-all-in "ab" "ÿÿ" #f (point) (point-max))"#,
        );
        assert_eq!(apply(text, &replacements(&cmds)), "ö-ab ÿÿ ÿÿ");
        // Within a region [3, 9): only the first two.
        let cmds = commands(&e, r#"(qr-replace-all-in "a(b)" "\\1\\1" #t 3 9)"#);
        assert_eq!(apply(text, &replacements(&cmds)), "ö-bb bb ab");
    }

    #[test]
    fn occur_collects_lines_and_lays_out_byte_offsets() {
        let e = engine_with("héllo Foo wörld\nnothing\nfoo bar FOO\n", 0);
        assert_eq!(
            value(&e, r#"(occur-collect (occur-fold-pattern "foo"))"#),
            r#"(3 ("src.txt" "" 0 "      1:" "héllo Foo wörld") ("src.txt" "" 2 "      3:" "foo bar FOO"))"#
        );
        // Header "é\n" is 3 bytes; the match after a multibyte char lands on
        // byte, not char, offsets; read-only ranges cover header+prefixes.
        assert_eq!(
            value(
                &e,
                r#"(results-layout (list "é" "occur-mode" "(?i)foo"
                     (list->vector (list (results-record "b" "" 0 "1:" "é foo")
                                         (results-record "b" "" 1 "2:" "FOO")))))"#
            ),
            r#"("é\n1:é foo\n2:FOO" ((12 14 "occur-prefix") (14 17 "occur-match") (3 5 "occur-prefix") (8 11 "occur-match")) ((0 5) (12 14)))"#
        );
    }

    #[test]
    fn results_parse_edits_detects_changes_and_broken_lines() {
        let e = engine_with("", 0);
        let spec = r#"(list "hdr" "occur-mode" "foo"
            (list->vector (list (results-record "b" "" 1 "     2:" "foo é")
                                (results-record "b" "" 3 "     4:" "zz"))))"#;
        assert_eq!(
            value(
                &e,
                &format!(r#"(results-parse-edits {spec} "hdr\n     2:FOO é\n     4:zz")"#)
            ),
            r#"((0 "FOO é"))"#
        );
        assert!(
            value(
                &e,
                &format!(r#"(results-parse-edits {spec} "hdr\n     2:x\nnew\n     4:zz")"#)
            )
            .contains("number of result lines changed")
        );
        assert!(
            value(
                &e,
                &format!(r#"(results-parse-edits {spec} "hdr\n  2:x\n     4:zz")"#)
            )
            .contains("Line 2 lost its prefix")
        );
    }

    #[test]
    fn results_apply_file_patches_lines_on_disk() {
        let dir = std::env::temp_dir().join(format!("macros-wgrep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        // Line 1 has trailing spaces + CR (grep shows it trimmed); line 2 is
        // multibyte; line 3 changed since the search and must be skipped.
        std::fs::write(&path, "keep\nfoo é  \r\nbär\nstale\n").unwrap();
        let e = engine_with("", 0);
        let p = path.to_str().unwrap();
        let res = value(
            &e,
            &format!(
                r#"(results-apply-file "{p}" (list (list 0 1 "foo é" "FOO É")
                                                  (list 1 2 "bär" "baz")
                                                  (list 2 3 "fresh" "x")))"#
            ),
        );
        assert_eq!(res, "((1 0) . 1)");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "keep\nFOO É  \r\nbaz\nstale\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn results_patch_buffer_line_uses_byte_offsets() {
        // Line 1 = "é foo  " (trailing spaces kept for a grep-trimmed view).
        let e = engine_with("ab\né foo  \nz", 0);
        let cmds = commands(&e, r#"(results-patch-buffer-line (list 0 1 "é foo" "ü"))"#);
        assert_eq!(replacements(&cmds), vec![(3, 9, "ü".to_string())]);
        // A line that no longer matches is left alone.
        let cmds = commands(&e, r#"(results-patch-buffer-line (list 0 2 "y" "q"))"#);
        assert!(replacements(&cmds).is_empty());
        // Records stay mutable so applied edits can be folded back in.
        assert_eq!(
            value(
                &e,
                r#"(begin (results-set! "*t*" "h" "occur-mode" "x" (list 1 2))
                          (vector-set! (results-records (results-spec "*t*")) 0 9)
                          (vector-ref (results-records (results-spec "*t*")) 0))"#
            ),
            "9"
        );
    }

    #[test]
    fn grep_records_use_relative_prefixes() {
        let e = engine_with("", 0);
        assert_eq!(
            value(
                &e,
                r#"(projectile-grep-record "/proj" "/proj/src/é.rs:12:  x: é")"#
            ),
            r#"("" "/proj/src/é.rs" 11 "src/é.rs:12:" "  x: é")"#
        );
    }

    #[test]
    fn replace_keys_are_bound() {
        let e = engine_with("", 0);
        let cmds = commands(&e, include_str!("../../scheme/replace.scm"));
        let bound = |chord: &str, cmd: &str| {
            cmds.iter().any(|c| {
                matches!(c, EditorCommand::DefineKey(m, k, v)
                    if m == "fundamental" && k == chord && v == cmd)
            })
        };
        assert!(bound("alt-%", "query-replace"));
        assert!(bound("ctrl-alt-%", "query-replace-regexp"));
        assert!(bound("ctrl-c p r", "projectile-replace"));
        for cmd in [
            "query-replace",
            "query-replace-regexp",
            "replace-string",
            "replace-regexp",
            "project-query-replace",
            "projectile-replace",
            "results-edit",
        ] {
            let q = format!(r#"(if (member "{cmd}" *interactive-commands*) "y" "n")"#);
            assert_eq!(value(&e, &q), r#""y""#, "{cmd} not registered");
        }
    }
}

/// Text plotting: scheme/plot.scm over the textplot-* primitives.
#[cfg(test)]
mod plot_tests {
    use super::*;

    fn engine_with(text: &str, point: usize) -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        engine.set_snapshot(EditorSnapshot {
            name: "notes.txt".into(),
            text: Rope::from_str(text),
            point,
            ..Default::default()
        });
        // Every span of a plot must lie inside its row, on char boundaries.
        engine
            .eval(
                r#"(define (plot-test-all? ok? xs)
                     (or (null? xs) (and (ok? (car xs)) (plot-test-all? ok? (cdr xs)))))
                   (define (plot-test-valid? p)
                     (plot-test-all?
                       (lambda (row)
                         (let ((n (utf8-length (plot-row-text row))))
                           (plot-test-all?
                             (lambda (s) (and (<= 0 (car s)) (< (car s) (cadr s))
                                              (<= (cadr s) n) (string? (nth s 2))))
                             (plot-row-spans row))))
                       p))"#,
            )
            .unwrap();
        engine
    }

    fn value(engine: &ScriptEngine, q: &str) -> String {
        match engine.eval(q) {
            Ok(r) => r.value.unwrap_or_default(),
            Err(e) => panic!("{q}: {e}"),
        }
    }

    #[test]
    fn sparkline_and_bars() {
        let e = engine_with("", 0);
        assert_eq!(
            value(&e, "(plot-sparkline-string (list 0 1 2 3 4 5 6 7))"),
            r#""▁▂▃▄▅▆▇█""#
        );
        assert_eq!(
            value(&e, "(plot-sparkline (list 1 2) 'face \"x\")"),
            r#"(("▁█" ((0 6 "x"))))"#
        );
        // Labels padded to the widest; bar at ⅛ resolution; value after.
        assert_eq!(
            value(
                &e,
                "(map plot-row-text (plot-bars (list (list \"a\" 4) (list \"bb\" 1)) 'width 4))"
            ),
            r#"("a  ████ 4" "bb █    1")"#
        );
        assert_eq!(
            value(
                &e,
                "(plot-test-valid? (plot-bars (list (list \"a\" 4) (list 'b 1.5)) 'colors #t))"
            ),
            "#true"
        );
    }

    #[test]
    fn insert_offsets_spans_by_bytes_after_multibyte_text() {
        // "héllo " is 7 bytes (é is 2) but 6 chars: spans must start at byte 7.
        let e = engine_with("héllo ", 7);
        let cmds = e
            .eval("(plot-insert (plot-sparkline (list 1 2)))")
            .unwrap()
            .commands;
        assert!(
            matches!(&cmds[0], EditorCommand::Insert(t) if t == "▁█"),
            "{cmds:?}"
        );
        assert!(
            matches!(&cmds[1], EditorCommand::OverlayFace { start: 7, end: 13, face } if face == "plot-series-0"),
            "{cmds:?}"
        );
    }

    #[test]
    fn line_and_scatter_frame_axes() {
        let e = engine_with("", 0);
        let rows = |q: &str| value(&e, &format!("(length {q})"));
        let lines = "(plot-lines (list (list \"a\" (list 0 1 2 3)) (list \"b\" (list 3 2 1 0))) 'width 20 'height 5 'title \"t\")";
        // title + 5 canvas rows + axis + x labels + legend
        assert_eq!(rows(lines), "9");
        assert_eq!(value(&e, &format!("(plot-test-valid? {lines})")), "#true");
        let text = value(&e, &format!("(plot-text {lines})"));
        assert!(text.contains('┤') && text.contains('└'), "{text}");
        assert!(text.contains("■ a") && text.contains("■ b"), "{text}");
        // Both series' faces are used.
        let spans = value(&e, &format!("(plot-spans {lines} 0)"));
        assert!(
            spans.contains("plot-series-0") && spans.contains("plot-series-1"),
            "{spans}"
        );

        // (x y) points; one named series → no legend by default.
        let scatter = "(plot-scatter (list (list \"s\" (list (list 0 0) (list 5 5) (list 10 2)))) 'width 10 'height 3)";
        assert_eq!(value(&e, &format!("(plot-test-valid? {scatter})")), "#true");
        assert_eq!(rows(scatter), "5");
        // Bare y-values are a single unnamed series with no legend.
        assert_eq!(rows("(plot-lines (list 5 5 5) 'width 10 'height 3)"), "5");
    }

    #[test]
    fn histogram_counts_land_in_bins() {
        let e = engine_with("", 0);
        let h = "(plot-histogram (list 0 0 0 0 1 1 9) 'bins 2 'bin-width 2 'height 2)";
        assert_eq!(value(&e, &format!("(plot-test-valid? {h})")), "#true");
        let text = value(&e, &format!("(plot-text {h})"));
        // 6 values in the low bin, 1 in the high bin; the count axis rounds
        // up to 10 so its top is labeled (6/10 and 1/10 of 16 eighths).
        let lines: Vec<&str> = text.trim_matches('"').split("\\n").collect();
        assert_eq!(
            lines,
            vec!["10 ┤▂   ", " 0 ┤█ ▂ ", "   └────", "    0  9"],
            "{text}"
        );
    }

    #[test]
    fn heatmap_cell_and_half_modes() {
        let e = engine_with("", 0);
        let cell = "(plot-heatmap (list (list 0 5) (list 10 #f)) 'row-labels (list \"r1\" \"r2\") 'col-labels (list \"a\" \"b\"))";
        assert_eq!(value(&e, &format!("(plot-test-valid? {cell})")), "#true");
        let spans = value(&e, &format!("(plot-spans {cell} 0)"));
        assert!(
            spans.contains("plot-heat-0-bg") && spans.contains("plot-heat-7-bg"),
            "{spans}"
        );
        // header + 2 rows + scale
        assert_eq!(value(&e, &format!("(length {cell})")), "4");

        let half = "(plot-heatmap (list (list 0 1) (list 1 0) (list 0 0)) 'mode 'half 'scale #f)";
        assert_eq!(value(&e, &format!("(plot-test-valid? {half})")), "#true");
        // 3 data rows → 2 text rows; the first pairs distinct top/bottom faces.
        assert_eq!(
            value(&e, &format!("(map plot-row-text {half})")),
            r#"("▀▀" "▀▀")"#
        );
        let spans = value(&e, &format!("(plot-spans {half} 0)"));
        assert!(spans.contains("plot-heat-7-bg"), "{spans}");
    }

    #[test]
    fn normal_pdf_and_cdf() {
        let e = engine_with("", 0);
        let num = |q: &str| value(&e, q).parse::<f64>().unwrap();
        assert!((num("(plot-normal-pdf 0 0 1)") - 0.398_942).abs() < 1e-5);
        assert!((num("(plot-normal-pdf 7 5 2)") - 0.120_985).abs() < 1e-5);
        assert!((num("(plot-normal-cdf 0 0 1)") - 0.5).abs() < 1e-6);
        assert!((num("(plot-normal-cdf 1.96 0 1)") - 0.975_002).abs() < 1e-6);
        assert!((num("(plot-normal-cdf -1 0 1)") - 0.158_655).abs() < 1e-6);
        assert!((num("(plot-normal-cdf 13 10 3)") - 0.841_345).abs() < 1e-6);
        assert_eq!(value(&e, "(plot--percent 0.6826894)"), r#""68.3%""#);
        assert_eq!(value(&e, "(plot--percent 0.99999)"), r#""100.0%""#);
    }

    #[test]
    fn function_plot_shades_under_the_first_curve() {
        let e = engine_with("", 0);
        // y = x over 0…10, shaded 5…10: the fill sits only in the right half.
        let p = "(plot-function (lambda (x) x) 0 10 'width 10 'height 2 'shade (list 5 10))";
        assert_eq!(value(&e, &format!("(plot-test-valid? {p})")), "#true");
        let text = value(&e, &format!("(plot-text {p})"));
        let lines: Vec<&str> = text.trim_matches('"').split("\\n").collect();
        // 2 canvas rows + axis + x labels; y axis rounded out to 0…10.
        assert_eq!(lines.len(), 4, "{text}");
        assert!(
            lines[0].starts_with("10 ┤") && lines[1].starts_with(" 0 ┤"),
            "{text}"
        );
        assert_eq!(lines[3], "    0       10", "{text}");
        let spans = value(&e, &format!("(plot-spans {p} 0)"));
        assert!(
            spans.contains("plot-shade") && spans.contains("plot-series-0"),
            "{spans}"
        );
        // Every plot-shade cell is in the right half of the canvas (col >= 5).
        let shaded = value(
            &e,
            &format!(
                "(map (lambda (row) (map car (filter (lambda (s) (equal? (nth s 2) \"plot-shade\")) (plot-row-spans row)))) {p})"
            ),
        );
        // Row prefix "10 ┤" is 2+1+3 = 6 bytes; braille cells are 3 bytes.
        for start in shaded
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|n| n.parse::<usize>().ok())
        {
            assert!(start >= 6 + 5 * 3, "shade at byte {start}: {shaded}");
        }

        // No shading → no plot-shade; a per-range face overrides the default.
        let plain = value(
            &e,
            "(plot-spans (plot-function (lambda (x) x) 0 1 'width 8 'height 2) 0)",
        );
        assert!(!plain.contains("plot-shade"), "{plain}");
        let custom = value(
            &e,
            "(plot-spans (plot-function (lambda (x) 1) 0 1 'width 8 'height 6 'shade (list (list 0 1 \"hot\"))) 0)",
        );
        assert!(custom.contains("hot"), "{custom}");
    }

    #[test]
    fn function_plot_skips_undefined_samples_and_multiple_functions() {
        let e = engine_with("", 0);
        // 1/x is ±inf at 0 and sqrt of a negative isn't real: both become gaps,
        // and the y range comes only from the finite samples.
        let p = "(plot-function (list (list \"inv\" (lambda (x) (if (= x 0) (/ 1.0 0.0) (/ 1.0 x)))) (list \"half\" (lambda (x) (if (< x 0) #f (* 0.5 x))))) -2 2 'width 12 'height 4)";
        assert_eq!(value(&e, &format!("(plot-test-valid? {p})")), "#true");
        let text = value(&e, &format!("(plot-text {p})"));
        assert!(text.contains("■ inv") && text.contains("■ half"), "{text}");
        let spans = value(&e, &format!("(plot-spans {p} 0)"));
        assert!(
            spans.contains("plot-series-0") && spans.contains("plot-series-1"),
            "{spans}"
        );
    }

    #[test]
    fn normal_plot_titles_ticks_and_probability() {
        let e = engine_with("", 0);
        let p = "(plot-normal 0 1 'width 49 'height 6 'shade (list -1 1))";
        assert_eq!(value(&e, &format!("(plot-test-valid? {p})")), "#true");
        let text = value(&e, &format!("(plot-text {p})"));
        let lines: Vec<&str> = text.trim_matches('"').split("\\n").collect();
        assert!(
            lines[0].contains("N(μ=0, σ=1)") && lines[0].contains("shaded: 68.3%"),
            "{text}"
        );
        // Whole-sigma ticks −4…4 along the axis, each marked ┬.
        let axis = lines[lines.len() - 2];
        assert_eq!(axis.matches('┬').count(), 9, "{text}");
        let labels = lines[lines.len() - 1];
        for t in ["-4", "-2", "0", "2", "4"] {
            assert!(labels.split_whitespace().any(|w| w == t), "{t}: {text}");
        }
        // y starts at 0 and tops out at a whole tick at or above the 0.399 peak.
        assert!(lines[1].trim_start().starts_with("0.4"), "{text}");

        // Two tails; ranges past the x range are clamped (P = 5%).
        let tails = value(
            &e,
            "(plot-text (plot-normal 100 15 'shade (list (list -1000 70.6) (list 129.4 1000))))",
        );
        assert!(
            tails.contains("N(μ=100, σ=15)") && tails.contains("shaded: 5.0%"),
            "{tails}"
        );
        // A caller's title wins over the generated one.
        let titled = value(&e, "(plot-text (plot-normal 0 1 'title \"IQ\"))");
        assert!(titled.contains("IQ") && !titled.contains("μ="), "{titled}");
    }

    #[test]
    fn frame_end_labels_still_separate_when_crowded() {
        let e = engine_with("", 0);
        assert_eq!(
            value(
                &e,
                "(plot--place-labels (plot--end-labels 4 \"-100\" \"200\"))"
            ),
            r#""-100 200""#
        );
        assert_eq!(
            value(
                &e,
                "(plot--place-labels (plot--centred-labels 10 (list (list 0 \"a\") (list 5 \"bb\") (list 9 \"c\"))))"
            ),
            r#""a   bb   c""#
        );
    }

    #[test]
    fn plots_tolerate_bad_data_and_ranges() {
        let e = engine_with("", 0);
        let valid = |q: &str| value(&e, &format!("(plot-test-valid? {q})"));
        // Infinite values are dropped, not crashes or squashed axes.
        assert_eq!(valid("(plot-histogram (list 1 2 (/ 1.0 0.0)))"), "#true");
        let lines = value(
            &e,
            "(plot-text (plot-lines (list 1 (/ 1.0 0.0) 3) 'width 10 'height 3))",
        );
        assert!(lines.contains('┤'), "y axis lost its labels: {lines}");
        // A pinned y-min above every data point, and a reversed x range.
        assert_eq!(
            valid("(plot-lines (list 1 2 3) 'y-min 5 'width 10 'height 3)"),
            "#true"
        );
        assert_eq!(
            valid("(plot-function sin 6 -6 'width 10 'height 3)"),
            "#true"
        );
        // All-equal values at a magnitude where ±1 rounds away.
        assert_eq!(
            valid("(plot-lines (list 1e17 1e17) 'width 10 'height 3)"),
            "#true"
        );
        // A normal with no spread is an error, not a blank chart.
        assert!(e.eval("(plot-normal 0 0)").is_err());
        assert!(e.eval("(plot-normal 0 -1)").is_err());
    }

    #[test]
    fn plot_options_are_normalised() {
        let e = engine_with("", 0);
        // Unsorted x-ticks still label left to right under their marks.
        let text = value(
            &e,
            "(plot-text (plot-function sin -6 6 'width 40 'height 3 'x-ticks (list 6 0 -6)))",
        );
        let lines: Vec<&str> = text.trim_matches('"').split("\\n").collect();
        let labels = lines[lines.len() - 1];
        let (a, b, c) = (
            labels.find("-6").unwrap(),
            labels.find(" 0 ").unwrap(),
            labels.rfind('6').unwrap(),
        );
        assert!(
            a < b && b < c && labels.chars().count() <= 4 + 40 + 1,
            "{text}"
        );
        // A reversed shade range is the same range.
        let rev = value(&e, "(plot-text (plot-normal 0 1 'shade (list 1.96 -1.96)))");
        assert!(rev.contains("shaded: 95.0%"), "{rev}");
        // The percentage covers the whole distribution, even off the plot.
        let off = value(
            &e,
            "(plot-text (plot-normal 0 1 'x-max 1 'shade (list 0 1000)))",
        );
        assert!(off.contains("shaded: 50.0%"), "{off}");
    }

    #[test]
    fn histogram_count_axis_and_flat_data() {
        let e = engine_with("", 0);
        // Tallest bin holds 1: the count axis steps by whole numbers.
        let text = value(
            &e,
            "(plot-text (plot-histogram (list 1 2 3) 'bins 3 'height 6))",
        );
        assert!(!text.contains("0.5"), "{text}");
        // All-equal values land mid-axis, labeled around the value.
        let flat = value(
            &e,
            "(map plot-row-text (plot-histogram (list 5 5 5) 'bins 3 'bin-width 1 'height 2))",
        );
        assert!(flat.contains(" █ ") || flat.contains("┤ █"), "{flat}");
        // Half-height heatmaps ignore 'row-labels instead of leaving a blank margin.
        let half = value(
            &e,
            "(map plot-row-text (plot-heatmap (list (list 0 1) (list 1 0)) 'mode 'half 'scale #f 'row-labels (list \"a\" \"b\")))",
        );
        assert_eq!(half, r#"("▀▀")"#);
    }

    #[test]
    fn themes_define_plot_faces_backgrounds_and_font() {
        for theme in ["macros-dark", "macros-light"] {
            let e = engine_with("", 0);
            let cmds = e
                .eval(crate::runtime::theme_script(theme).unwrap())
                .unwrap()
                .commands;
            let font = cmds.iter().filter(|c| matches!(c, EditorCommand::SetFaceFont(f, fam) if f.starts_with("plot-") && fam == "Macros Plot")).count();
            let bg = cmds.iter().filter(|c| matches!(c, EditorCommand::SetFaceBackground(f, _) if f.starts_with("plot-heat-"))).count();
            assert_eq!((font, bg), (15, 8), "{theme}");
        }
    }

    #[test]
    fn demo_renders_and_face_background_is_a_command() {
        let e = engine_with("", 0);
        let cmds = e.eval("(plot-demo)").unwrap().commands;
        assert!(
            cmds.iter().any(
                |c| matches!(c, EditorCommand::RenderBuffer { name, .. } if name == "*plot-demo*")
            ),
            "{cmds:?}"
        );
        let cmds = e
            .eval("(set-face-background \"x-bg\" \"#123456\")")
            .unwrap()
            .commands;
        assert!(
            matches!(&cmds[..], [EditorCommand::SetFaceBackground(f, c)] if f == "x-bg" && c == "#123456")
        );
    }
}

/// Audit fixes in shell/magit/lsp/helm/helix/evil scheme files: shell quoting,
/// `~` expansion, inlay-hint grouping, range merging, screen-line bounds,
/// path-vs-buffer-name lookups.
#[cfg(test)]
mod audit_scheme_fix_tests {
    use super::*;

    fn engine_with(text: &str, point: usize, path: &str, visible: (usize, usize)) -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        engine.set_snapshot(EditorSnapshot {
            name: "mod.rs<parser>".into(),
            path: path.into(),
            text: Rope::from_str(text),
            point,
            selections: vec![(point, point)],
            visible_lines: visible,
            ..Default::default()
        });
        engine
    }

    fn value(engine: &ScriptEngine, q: &str) -> String {
        match engine.eval(q) {
            Ok(r) => r.value.unwrap_or_default(),
            Err(e) => panic!("{q}: {e}"),
        }
    }

    #[test]
    fn shell_quote_survives_spaces_and_single_quotes() {
        let e = engine_with("", 0, "", (0, 1));
        assert_eq!(value(&e, r#"(shell-quote "a b")"#), r#""'a b'""#);
        // Round-trip through a real shell.
        let out = value(
            &e,
            r#"(call-process "/bin/sh" (list "-c" (string-append "printf %s " (shell-quote "it's a $x; b"))))"#,
        );
        assert_eq!(out, r#""it's a $x; b""#);
    }

    #[test]
    fn expand_file_name_only_treats_leading_tilde_segment_as_home() {
        let e = engine_with("", 0, "", (0, 1));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            value(&e, r#"(expand-file-name "/a/b/foo~bar")"#),
            r#""/a/b/foo~bar""#
        );
        assert_eq!(
            value(&e, r#"(expand-file-name "/a/backup.txt~")"#),
            r#""/a/backup.txt~""#
        );
        assert_eq!(
            value(&e, r#"(expand-file-name "/a/b/~/x")"#),
            format!("\"{home}/x\"")
        );
        assert_eq!(
            value(&e, r#"(expand-file-name "~")"#),
            format!("\"{home}/\"")
        );
        assert_eq!(
            value(&e, r#"(expand-file-name "~/")"#),
            format!("\"{home}/\"")
        );
        assert_eq!(value(&e, r#"(expand-file-name "/a/b//etc")"#), r#""/etc""#);
    }

    #[test]
    fn inlay_hints_on_one_line_are_joined_in_character_order() {
        let e = engine_with("", 0, "", (0, 1));
        assert_eq!(
            value(
                &e,
                r#"(lsp--inlay-group (list (list 2 9 ": b") (list 0 1 "x") (list 2 3 ": a")))"#
            ),
            r#"((0 . "  x") (2 . "  : a  : b"))"#
        );
    }

    #[test]
    fn helix_merge_ranges_merges_overlapping_and_touching() {
        let e = engine_with("", 0, "", (0, 1));
        assert_eq!(
            value(
                &e,
                "(helix-merge-ranges (list (cons 0 5) (cons 3 8) (cons 8 9) (cons 12 14)))"
            ),
            "((0 . 9) (12 . 14))"
        );
    }

    #[test]
    fn screen_bottom_excludes_overscan_rows() {
        let text: String = (0..200).map(|i| format!("l{i}\n")).collect();
        // Viewport of ~30.5 rows at the top: Rust reports (0, 32).
        let e = engine_with(&text, 0, "", (0, 32));
        assert_eq!(value(&e, "(evil-visible-bottom (visible-lines))"), "29");
        assert_eq!(value(&e, r#"(evil-screen-line "L" 1)"#), "29");
        assert_eq!(value(&e, r#"(evil-screen-line "H" 1)"#), "0");
        // Clamped at the buffer end: the last real line is the bottom.
        let e = engine_with("a\nb\nc\n", 0, "", (0, 4));
        assert_eq!(value(&e, "(evil-visible-bottom (visible-lines))"), "2");
    }

    #[test]
    fn file_lookups_use_path_not_uniquified_buffer_name() {
        let e = engine_with("", 0, "/src/parser/mod.rs", (0, 1));
        assert_eq!(value(&e, "(lsp--file-ext (lsp--buffer-path))"), r#""rs""#);
        assert_eq!(value(&e, "(magit-blame-file)"), r#""/src/parser/mod.rs""#);
        let e = engine_with("", 0, "/notes/a.md", (0, 1));
        assert_eq!(value(&e, "(rainbow--skip?)"), "#true");
        // Fileless buffers fall back to the name.
        let e = engine_with("", 0, "", (0, 1));
        assert_eq!(value(&e, "(lsp--buffer-path)"), r#""mod.rs<parser>""#);
    }
}

/// Regression tests for the Scheme audit fixes (markdown / editing / org /
/// org-roam / folding / persistence / themes).
#[cfg(test)]
mod scheme_audit_fixes {
    use super::*;

    fn engine_at(name: &str, path: &str, text: &str, point: usize, line: usize) -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        set_text(&engine, name, path, text, point, line);
        engine
    }

    fn set_text(
        engine: &ScriptEngine,
        name: &str,
        path: &str,
        text: &str,
        point: usize,
        line: usize,
    ) {
        engine.set_snapshot(EditorSnapshot {
            name: name.into(),
            path: path.into(),
            text: Rope::from_str(text),
            point,
            line,
            selections: vec![(point, point)],
            ..Default::default()
        });
    }

    fn v(engine: &ScriptEngine, q: &str) -> String {
        engine.eval(q).unwrap().value.unwrap()
    }

    #[test]
    fn md_wrap_region_lands_between_delimiters() {
        let e = engine_at("a.md", "/tmp/a.md", "abc", 1, 0);
        let cmds = e.eval(r#"(md-wrap-region "**" "**")"#).unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(3))),
            "{cmds:?}"
        );
    }

    #[test]
    fn sort_lines_keeps_trailing_newline() {
        let e = engine_at("a.txt", "", "", 0, 0);
        assert_eq!(v(&e, r#"(sort-lines-text "b\na\n")"#), r#""a\nb\n""#);
        assert_eq!(v(&e, r#"(sort-lines-text "b\na")"#), r#""a\nb""#);
    }

    #[test]
    fn org_export_inline_protects_links_and_dollar_groups() {
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        assert_eq!(
            v(&e, r#"(org--inline->md "/it/ [[https://a.com/b/c][d]]")"#),
            r#""_it_ [d](https://a.com/b/c)""#
        );
        let html = v(&e, r#"(org--inline->html "[[https://a.com/b/c]] *b* /i/")"#);
        assert!(html.contains(r#"href=\"https://a.com/b/c\""#), "{html}");
        assert!(html.contains("<strong>b</strong>"), "{html}");
        assert!(html.contains("<em>i</em>"), "{html}");
        let ascii = v(&e, r#"(org--inline->ascii "[[https://a.com/x/y][d]] /i/")"#);
        assert_eq!(ascii, r#""d (https://a.com/x/y) i""#);
    }

    #[test]
    fn user_text_replacements_keep_dollars() {
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        assert_eq!(
            v(&e, r#"(org--expand-template-final "* %?" "cost $5 ${x}")"#),
            r#""* cost $5 ${x}""#
        );
        assert_eq!(
            v(
                &e,
                r##"(org-roam--expand-tmpl "#+title: ${title}" "A $1 b" "s" "i")"##
            ),
            r##""#+title: A $1 b""##
        );
        assert_eq!(
            v(
                &e,
                r#"(org-roam--rewrite-links "[[id:abc][old]]" "abc" "New $2")"#
            ),
            r#""[[id:abc][New $2]]""#
        );
    }

    #[test]
    fn org_heading_title_strips_bare_keyword() {
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        assert_eq!(v(&e, r#"(nth (org--parse-headline "* TODO") 3)"#), r#""""#);
        assert_eq!(
            v(&e, r#"(nth (org--parse-headline "* TODO [#A]") 3)"#),
            r#""""#
        );
        assert_eq!(
            v(&e, r#"(nth (org--parse-headline "* TODO x") 3)"#),
            r#""x""#
        );
    }

    #[test]
    fn org_move_subtree_down_needs_a_true_sibling() {
        // "** B" is followed only by its parent's sibling "* C": no move.
        let e = engine_at("a.org", "/tmp/a.org", "* A\n** B\n* C\n", 4, 1);
        let cmds = e.eval("(org-move-subtree-down)").unwrap().commands;
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::ReplaceRegion { .. })),
            "{cmds:?}"
        );
    }

    #[test]
    fn uniquified_buffers_detected_by_file_path() {
        let e = engine_at("notes.org<x>", "/tmp/x/notes.org", "* A\n", 0, 0);
        assert_eq!(v(&e, "(if (org--active?) 1 0)"), "1");
        assert_eq!(v(&e, "(org--current-file-path)"), r#""/tmp/x/notes.org""#);
        assert_eq!(v(&e, r#"(org--output-path ".md")"#), r#""/tmp/x/notes.md""#);
        set_text(&e, "README.md<y>", "/tmp/y/README.md", "# A\n", 0, 0);
        assert_eq!(v(&e, "(if (md--active?) 1 0)"), "1");
    }

    #[test]
    fn saveplace_keys_on_file_path() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let e = engine_at("Cargo.toml<m>", path, "0123456789", 5, 0);
        e.eval("(saveplace-remember)").unwrap();
        assert_eq!(v(&e, &format!("(alist-ref *saveplace* {path:?})")), "5");
    }

    #[test]
    fn org_column_formula_skips_header_rows() {
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        assert_eq!(
            v(
                &e,
                r#"(org--table-first-data-row (list "| a | b |" "|---+---|" "| 1 | 2 |"))"#
            ),
            "2"
        );
        assert_eq!(
            v(&e, r#"(org--table-first-data-row (list "| 1 | 2 |"))"#),
            "1"
        );
        let g =
            r#"(org--apply-formula-from (list (list "a" "b" "c") (list "1" "2" "")) "$3=$1+$2" 2)"#;
        assert_eq!(v(&e, &format!("(nth (car {g}) 2)")), r#""c""#);
        assert_eq!(v(&e, &format!("(nth (cadr {g}) 2)")), r#""3""#);
    }

    #[test]
    fn org_keywords_may_hold_hyphens_and_digits() {
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        e.eval(r#"(set! *org-custom-keywords* (list "IN-PROGRESS" "P1"))"#)
            .unwrap();
        assert_eq!(
            v(
                &e,
                r#"(org--parse-headline "* IN-PROGRESS [#B] Ship it :t:")"#
            ),
            r#"("*" "IN-PROGRESS" "B" "Ship it" "t")"#
        );
        assert_eq!(v(&e, r#"(org--leading-keyword "P1 later")"#), r#""P1""#);
        // An unknown hyphenated word stays part of the title.
        assert_eq!(
            v(&e, r#"(nth (org--parse-headline "* NOT-ONE x") 3)"#),
            r#""NOT-ONE x""#
        );
    }

    #[test]
    fn org_line_edits_keep_point() {
        // Point on the `l` of "Child" (byte 6): C-c C-t inserts "TODO " before
        // it, and point follows the letter instead of jumping to the line end.
        let e = engine_at("a.org", "/tmp/a.org", "** Child two\n", 6, 0);
        let cmds = e.eval("(org-todo)").unwrap().commands;
        assert!(
            matches!(cmds.last(), Some(EditorCommand::GotoChar(11))),
            "{cmds:?}"
        );
        // A planning line inserted below the heading leaves point on it.
        let e = engine_at("a.org", "/tmp/a.org", "* TODO x\nbody\n", 3, 0);
        let cmds = e
            .eval(r#"(org--plan-apply 0 "SCHEDULED" "2026-01-05")"#)
            .unwrap()
            .commands;
        assert!(
            cmds.iter().any(|c| matches!(c, EditorCommand::GotoChar(3))),
            "{cmds:?}"
        );
        // The mapping itself: before / after / at an insertion / multi-line.
        assert_eq!(
            v(&e, r#"(org--point-after-edit 2 5 8 "abc" "abcdef")"#),
            "2"
        );
        assert_eq!(
            v(&e, r#"(org--point-after-edit 9 5 8 "abc" "abcdef")"#),
            "12"
        );
        assert_eq!(v(&e, r#"(org--point-after-edit 5 5 5 "" "xyz")"#), "5");
        // Row 1, column 2 of a re-aligned table stays at row 1, column 2.
        assert_eq!(
            v(
                &e,
                r#"(org--point-after-edit 8 0 11 "| a |\n| b |" "| aa |\n| bb |")"#
            ),
            "9"
        );
    }

    #[test]
    fn org_footnote_reference_stays_at_point() {
        let e = engine_at("a.org", "/tmp/a.org", "see\n", 3, 0);
        let cmds = e.eval("(org-footnote-new)").unwrap().commands;
        let insert = cmds
            .iter()
            .position(|c| matches!(c, EditorCommand::Insert(t) if t == "[fn:1]"));
        let def = cmds.iter().position(
            |c| matches!(c, EditorCommand::InsertAt { pos: 10, text } if text == "\n[fn:1] "),
        );
        assert!(
            matches!((insert, def), (Some(i), Some(d)) if i < d),
            "{cmds:?}"
        );
    }

    #[test]
    fn org_inline_babel_replaces_previous_result() {
        let line = "src_scheme{(+ 1 2)} {{{results(old)}}} x\n";
        let e = engine_at("a.org", "/tmp/a.org", line, 0, 0);
        let cmds = e.eval("(org-babel-execute-inline)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c,
                EditorCommand::ReplaceRegion { start: 19, end: 38, text }
                    if text == " {{{results(3)}}}")),
            "{cmds:?}"
        );
    }

    #[test]
    fn org_agenda_scan_and_tag_match() {
        let text =
            "* Notes :ref:\n* TODO t\n  DEADLINE: <2026-01-02 Fri> SCHEDULED: <2026-01-01 Thu>\n";
        let e = engine_at("a.org", "/tmp/a.org", text, 0, 0);
        // The agenda lists both dates of `t`; a plain note is not an entry.
        assert_eq!(v(&e, r#"(length (org--scan-file "/tmp/a.org"))"#), "2");
        assert_eq!(
            v(
                &e,
                r#"(map (lambda (x) (nth x 3)) (org--scan-file "/tmp/a.org"))"#
            ),
            r#"("SCHEDULED" "DEADLINE")"#
        );
        // The tags view sees every heading once, the tagged note included.
        assert_eq!(v(&e, r#"(length (org--scan-file "/tmp/a.org" #t))"#), "2");
        assert_eq!(
            v(&e, r#"(nth (car (org--scan-file "/tmp/a.org" #t)) 6)"#),
            r#"("ref")"#
        );
        // "-" separates terms like "+" does: work AND NOT done.
        assert_eq!(
            v(&e, r#"(if (org--tag-match "work-done" (list "work")) 1 0)"#),
            "1"
        );
        assert_eq!(
            v(
                &e,
                r#"(if (org--tag-match "work-done" (list "work" "done")) 1 0)"#
            ),
            "0"
        );
        assert_eq!(
            v(&e, r#"(key-binding "evil-normal:org-agenda-mode" "j")"#),
            r#""next-line""#
        );
    }

    #[test]
    fn org_archive_and_capture_placement() {
        // ARCHIVE_TIME goes after the planning line, into an existing drawer.
        assert_eq!(
            v(
                &engine_at("a.org", "/tmp/a.org", "", 0, 0),
                r#"(org--block-add-property (list "* A" "  SCHEDULED: <x>" "body") ":K: v")"#
            ),
            r#"("* A" "  SCHEDULED: <x>" "  :PROPERTIES:" "  :K: v" "  :END:" "body")"#
        );
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        assert_eq!(
            v(
                &e,
                r#"(org--block-add-property (list "* A" ":PROPERTIES:" ":ID: 1" ":END:" "b") ":K: v")"#
            ),
            r#"("* A" ":PROPERTIES:" ":ID: 1" "  :K: v" ":END:" "b")"#
        );
        // A capture filed under a level-2 heading becomes its child.
        assert_eq!(
            v(&e, r#"(org--as-child-lines "* x\nnote" 2)"#),
            r#"("*** x" "note")"#
        );
        // %T carries the time of day, %t only the date.
        assert_eq!(
            v(
                &e,
                r#"(if (>= (string-match "^<[0-9-]+ [A-Za-z]+ [0-9]{2}:[0-9]{2}>$" (org--expand-template "%T")) 0) 1 0)"#
            ),
            "1"
        );
    }

    #[test]
    fn org_export_skips_metadata() {
        let e = engine_at("a.org", "/tmp/a.org", "", 0, 0);
        assert_eq!(
            v(
                &e,
                r#"(org-parse "* H\n  SCHEDULED: <2026-01-01 Thu>\n  :PROPERTIES:\n  :ID: x\n  :END:\nbody\n: out")"#
            ),
            r#"(("heading" 1 "" "" "H" "") ("para" "body") ("example" "out"))"#
        );
        // LaTeX escapes link text but not the URL's `_`.
        assert_eq!(
            v(&e, r#"(org--inline->latex "[[http://x.com/a_b][l_k]]")"#),
            r#""\\href{http://x.com/a_b}{l\\_k}""#
        );
        // M-RET on a checkbox item starts an unchecked one.
        let e = engine_at("a.org", "/tmp/a.org", "- [X] a\n", 0, 0);
        let cmds = e.eval("(org-insert-item)").unwrap().commands;
        assert!(
            cmds.iter().any(|c| matches!(c,
                EditorCommand::InsertAt { pos: 7, text } if text == "\n- [ ] ")),
            "{cmds:?}"
        );
    }

    #[test]
    fn fold_state_follows_edits() {
        let e = engine_at("a.org", "/tmp/a.org", "* A\nbody\n* B\nbody\n", 0, 0);
        assert_eq!(
            v(
                &e,
                r#"(fold-anchor-locate (list "x" "* A" "y") (cons 0 "* A"))"#
            ),
            "1"
        );
        assert_eq!(
            v(
                &e,
                r#"(if (fold-anchor-locate (list "x") (cons 0 "* A")) 1 0)"#
            ),
            "0"
        );
        // Fold "* B" (line 2), then insert a line above it: the state follows.
        e.eval("(org--set-heading-state 2 'folded)").unwrap();
        set_text(
            &e,
            "a.org",
            "/tmp/a.org",
            "new\n* A\nbody\n* B\nbody\n",
            0,
            0,
        );
        e.eval("(org--fold-state-resync)").unwrap();
        assert_eq!(v(&e, "(car (car (org--fold-state-get)))"), "3");
        // Markdown: same relocation.
        set_text(&e, "a.md", "/tmp/a.md", "# A\nb\n# B\nb\n", 0, 0);
        e.eval("(md--folded-set (list 2))").unwrap();
        set_text(&e, "a.md", "/tmp/a.md", "x\n# A\nb\n# B\nb\n", 0, 0);
        e.eval("(md--folded-resync (md-lines))").unwrap();
        assert_eq!(v(&e, "(car (md--folded-get))"), "3");
        // Generic indentation folding: same relocation.
        set_text(&e, "a.py", "/tmp/a.py", "def f():\n  x\n", 0, 0);
        e.eval("(fold--folded-set (list 0))").unwrap();
        set_text(&e, "a.py", "/tmp/a.py", "import y\ndef f():\n  x\n", 0, 0);
        e.eval("(fold--resync)").unwrap();
        assert_eq!(v(&e, "(car (fold--folded-get))"), "1");
    }

    #[test]
    fn org_roam_commands_callable_without_args() {
        let e = engine_at("*scratch*", "", "", 0, 0);
        e.eval("(org-roam-set-directory)").unwrap();
        assert_eq!(
            v(&e, r#"(key-binding "org-roam-backlinks-mode" "g")"#),
            r#""org-roam-backlinks-refresh""#
        );
    }

    #[test]
    fn themes_define_module_faces() {
        for theme in ["macros-dark", "macros-light"] {
            let e = engine_at("a", "", "", 0, 0);
            let cmds = e
                .eval(crate::runtime::theme_script(theme).unwrap())
                .unwrap()
                .commands;
            for face in [
                "org-level-1",
                "org-todo",
                "org-link",
                "tabulated-header",
                "projectile-modeline",
            ] {
                assert!(
                    cmds.iter()
                        .any(|c| matches!(c, EditorCommand::SetFace(f, _) if f == face)),
                    "{theme} missing {face}"
                );
            }
        }
    }
}

#[cfg(test)]
mod audit_fixes {
    use super::*;

    #[test]
    fn directory_of_bare_name_is_current_dir() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        assert_eq!(val(r#"(directory-of "foo.txt")"#), "\"./\"");
        assert_eq!(val(r#"(directory-of "/a/b.txt")"#), "\"/a/\"");
        assert_eq!(val(r#"(directory-of "/a/")"#), "\"/a/\"");
    }

    /// Large stdin + large stdout must not deadlock (stdin is written on its
    /// own thread while output is drained).
    #[cfg(unix)]
    #[test]
    fn call_process_full_streams_large_input_and_output() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        engine
            .eval(r#"(define r (call-process-full "cat" '() (make-string 300000 #\a)))"#)
            .unwrap();
        assert_eq!(val("(car r)"), "\"0\"");
        assert_eq!(val("(string-length (cadr r))"), "300000");
    }

    #[test]
    fn re_search_forward_keeps_context_before_point() {
        let text = "foo bar\nbaz";
        // `^` at point (mid-line) must not match: the line starts earlier.
        assert_eq!(re_search_forward_in(text, 4, "^b"), Some((8, 9)));
        // `\b` sees the character before point.
        assert_eq!(re_search_forward_in(text, 5, r"\bar"), None);
        assert_eq!(re_search_forward_in(text, 0, r"ba."), Some((4, 7)));
        assert_eq!(re_search_forward_in(text, 0, "("), None);
        assert_eq!(re_search_forward_in(text, 99, "a"), None);
    }

    #[test]
    fn resolv_conf_lists_every_parseable_nameserver() {
        let conf = "# c\nnameserver 2001:4860:4860::8888\nnameserver fe80::1%en0\n\
                    nameserver 1.1.1.1 # trailing\nsearch lan\n";
        let ns = parse_resolv_conf_nameservers(conf);
        assert_eq!(
            ns,
            vec![
                "2001:4860:4860::8888".parse::<std::net::IpAddr>().unwrap(),
                "1.1.1.1".parse().unwrap(),
            ]
        );
    }

    /// A read timeout mid-line/mid-length keeps the bytes already received;
    /// the next receive returns them with the rest.
    #[test]
    fn socket_recv_timeout_keeps_partial_data() {
        let (engine, _rx) = ScriptEngine::new();
        let val = |src: &str| engine.eval(src).unwrap().value.unwrap();
        engine
            .eval(r#"(define srv (tcp-listen "127.0.0.1" 0))"#)
            .unwrap();
        let port: u16 = val("(tcp-listener-port srv)").parse().unwrap();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"hel").unwrap();
            go_rx.recv().unwrap();
            c.write_all(b"lo\nAB").unwrap();
            go_rx.recv().unwrap();
            c.write_all(b"CD").unwrap();
            go_rx.recv().unwrap();
        });
        engine.eval("(define conn (tcp-accept srv))").unwrap();
        assert!(val("(socket-set-timeout conn 200)").contains("#t"));
        assert_eq!(val("(socket-recv-line conn)"), "\"\"");
        go_tx.send(()).unwrap();
        assert_eq!(val("(socket-recv-line conn)"), "\"hello\\n\"");
        // "AB" arrived; asking for 4 times out, then completes with "CD".
        assert_eq!(val("(socket-recv-bytes conn 4)"), "\"\"");
        go_tx.send(()).unwrap();
        assert_eq!(val("(socket-recv-bytes conn 4)"), "\"ABCD\"");
        go_tx.send(()).unwrap();
        client.join().unwrap();
        assert!(val("(socket-close conn)").contains("#t"));
        assert!(val("(tcp-listener-close srv)").contains("#t"));
    }
}

/// Secret-handling primitives: `read-passwd` (masked read + one-shot
/// callbacks), the child-env spawners, and `with-process-env`.
#[cfg(test)]
mod secrets_primitives {
    use super::*;

    fn core_engine() -> (ScriptEngine, UnboundedReceiver<ScriptOutcome>) {
        let (engine, rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        (engine, rx)
    }

    fn val(engine: &ScriptEngine, src: &str) -> String {
        engine.eval(src).unwrap().value.unwrap_or_default()
    }

    /// `call-process-env` layers "KEY=VALUE" over the inherited env (split at
    /// the first `=`), and a bare "KEY" unsets — for the child only.
    #[cfg(unix)]
    #[test]
    fn call_process_env_sets_and_unsets_child_env() {
        let (engine, _rx) = ScriptEngine::new();
        engine
            .eval(
                r#"(define r (call-process-env "/bin/sh"
                     (list "-c" "printf '%s|%s' \"$MACROS_T1\" \"${HOME-unset}\"")
                     "" (list "MACROS_T1=a=b c" "HOME")))"#,
            )
            .unwrap();
        assert_eq!(val(&engine, "(car r)"), "\"0\"");
        assert_eq!(val(&engine, "(cadr r)"), "\"a=b c|unset\"");
        assert!(std::env::var("MACROS_T1").is_err());
    }

    /// `with-process-env` reaches the sync spawners inside its extent, nests
    /// (inner wins, explicit env wins over both), and is gone afterwards.
    #[cfg(unix)]
    #[test]
    fn with_process_env_scopes_sync_spawns() {
        let (engine, _rx) = core_engine();
        let sh = r#"(lambda () (cadr (call-process-full "/bin/sh" (list "-c" "printf %s \"${MACROS_W-none}\"") "")))"#;
        assert_eq!(
            val(
                &engine,
                &format!(r#"(with-process-env (list "MACROS_W=outer") {sh})"#)
            ),
            "\"outer\""
        );
        assert_eq!(
            val(
                &engine,
                &format!(
                    r#"(with-process-env (list "MACROS_W=outer")
                         (lambda () (with-process-env (list "MACROS_W=inner") {sh})))"#
                )
            ),
            "\"inner\""
        );
        assert_eq!(
            val(
                &engine,
                r#"(with-process-env (list "MACROS_W=dyn")
                     (lambda () (cadr (call-process-env "/bin/sh"
                       (list "-c" "printf %s \"$MACROS_W\"") "" (list "MACROS_W=explicit")))))"#
            ),
            "\"explicit\""
        );
        // The extent closed: nothing leaks into a later spawn.
        assert_eq!(val(&engine, &format!("({sh})")), "\"none\"");
        // call-process (stdout-only) honours it too.
        assert_eq!(
            val(
                &engine,
                r#"(with-process-env (list "MACROS_W=cp")
                     (lambda () (call-process "/bin/sh" (list "-c" "printf %s \"$MACROS_W\""))))"#
            ),
            "\"cp\""
        );
    }

    /// An error escaping the thunk skips the pop — the engine's per-job reset
    /// must still keep the override out of the NEXT job.
    #[cfg(unix)]
    #[test]
    fn with_process_env_error_does_not_leak_into_next_job() {
        let (engine, _rx) = core_engine();
        assert!(
            engine
                .eval(r#"(with-process-env (list "MACROS_L=leak") (lambda () (car '())))"#)
                .is_err()
        );
        assert_eq!(
            val(
                &engine,
                r#"(cadr (call-process-full "/bin/sh" (list "-c" "printf %s \"${MACROS_L-none}\"") ""))"#
            ),
            "\"none\""
        );
    }

    /// Async spawners capture the extent's env into their command (the spawn
    /// itself happens later on the UI thread); explicit env layers on top.
    #[test]
    fn async_spawners_capture_env() {
        let (engine, _rx) = core_engine();
        let cmds = engine
            .eval(
                r#"(with-process-env (list "A=dyn" "B=dyn")
                     (lambda ()
                       (make-process-env "psql" (list) "on-line" "" (list "B=explicit"))
                       (make-terminal "*t*" "" (list) "")
                       (dap-start "dlv" (list) "" "on-msg" "")
                       (set-lsp "go" "gopls")))"#,
            )
            .unwrap()
            .commands;
        let pairs = |e: &EnvOverrides| e.0.clone();
        let dyn_then = vec![
            ("A".to_string(), Some("dyn".to_string())),
            ("B".to_string(), Some("dyn".to_string())),
        ];
        let mut seen = 0;
        for c in &cmds {
            match c {
                EditorCommand::MakeProcess(p) => {
                    let mut want = dyn_then.clone();
                    want.push(("B".into(), Some("explicit".into())));
                    assert_eq!(pairs(&p.env), want);
                    seen += 1;
                }
                EditorCommand::MakeTerminal(t) => {
                    assert_eq!(pairs(&t.env), dyn_then);
                    seen += 1;
                }
                EditorCommand::DapStart(a) => {
                    assert_eq!(pairs(&a.env), dyn_then);
                    seen += 1;
                }
                // The language server is spawned much later (on first visit
                // of a Go file), but with the env of the configuring extent.
                EditorCommand::SetLsp(_, _, env) => {
                    assert_eq!(pairs(env), dyn_then);
                    seen += 1;
                }
                _ => {}
            }
        }
        assert_eq!(seen, 4, "{cmds:?}");
        // Debug never shows the values.
        assert!(!format!("{cmds:?}").contains("explicit"));
    }

    /// `async-lane-run`'s optional trailing env rides on its MakeProcess.
    #[test]
    fn async_lane_run_passes_env() {
        let (engine, _rx) = core_engine();
        engine
            .eval(include_str!("../../scheme/lib-async.scm"))
            .unwrap();
        match engine
            .eval(r#"(async-lane-run "psql" "/bin/sh" (list "-c" "psql") 'on-line 'on-done (list "PGPASSWORD=pw"))"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::MakeProcess(p)] => {
                assert_eq!(p.env.0, vec![("PGPASSWORD".to_string(), Some("pw".to_string()))]);
            }
            other => panic!("expected MakeProcess, got {other:?}"),
        }
    }

    /// `read-passwd` emits ReadPasswd; on-cancel is optional.
    #[test]
    fn read_passwd_parses() {
        let (engine, _rx) = core_engine();
        match engine
            .eval(r#"(read-passwd "Password: " 'got-pw)"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [
                EditorCommand::ReadPasswd {
                    prompt,
                    callback,
                    on_cancel,
                },
            ] => {
                assert_eq!(prompt, "Password: ");
                assert_eq!(callback, "got-pw");
                assert!(on_cancel.is_none());
            }
            other => panic!("expected ReadPasswd, got {other:?}"),
        }
        match engine
            .eval(r#"(read-passwd "Password: " 'got-pw 'gave-up)"#)
            .unwrap()
            .commands
            .as_slice()
        {
            [EditorCommand::ReadPasswd { on_cancel, .. }] => {
                assert_eq!(on_cancel.as_deref(), Some("gave-up"));
            }
            other => panic!("expected ReadPasswd, got {other:?}"),
        }
    }

    /// Closure callbacks are one-shot as a group: firing either frees both.
    #[test]
    fn read_passwd_closures_are_freed_after_either_fires() {
        let (engine, _rx) = core_engine();
        engine.eval("(define got #f)").unwrap();
        let names = |src: &str| match engine.eval(src).unwrap().commands.as_slice() {
            [
                EditorCommand::ReadPasswd {
                    callback,
                    on_cancel,
                    ..
                },
            ] => (callback.clone(), on_cancel.clone().unwrap()),
            other => panic!("expected ReadPasswd, got {other:?}"),
        };
        let (ok, cancel) = names(
            r#"(read-passwd "pw: " (lambda (pw) (set! got pw)) (lambda (_) (set! got 'cancelled)))"#,
        );
        let before = val(&engine, "(hash-length *thunk-table*)");
        engine.call_action(&ok, Some(" s3cret "));
        assert_eq!(val(&engine, "got"), "\" s3cret \"");
        assert_eq!(
            val(&engine, "(hash-length *thunk-table*)")
                .parse::<i64>()
                .unwrap(),
            before.parse::<i64>().unwrap() - 2,
            "both closures freed"
        );
        // The sibling is now a no-op.
        engine.call_action(&cancel, Some(""));
        assert_eq!(val(&engine, "got"), "\" s3cret \"");
    }

    /// A failing secret callback reports an error that does NOT contain the
    /// secret (Steel's own message would quote it).
    #[test]
    fn secret_callback_error_hides_the_value() {
        let (engine, mut rx) = core_engine();
        engine.eval("(define (bad-cb pw) (+ pw 1))").unwrap();
        engine.dispatch_secret_action("bad-cb", "hunter2-secret");
        let outcome = futures::executor::block_on(futures::StreamExt::next(&mut rx)).unwrap();
        match outcome {
            ScriptOutcome::Commands { message, .. } => {
                let m = message.unwrap();
                assert!(m.contains("bad-cb") && !m.contains("hunter2"), "{m}");
            }
            _ => panic!("expected a Commands outcome"),
        }
    }
}

/// scheme/secrets.scm end to end through real gpg: a miss prompts, the first
/// save creates authinfo.gpg (new passphrase, confirmed), a locked store asks
/// for the passphrase once and then answers from memory, concurrent requests
/// queue, and `secret-forget` makes the next lookup prompt again. The store
/// file lands in the per-thread test persistence dir. Skipped without gpg.
#[cfg(test)]
mod secrets_store {
    use super::*;

    fn gpg_available() -> bool {
        [
            "/opt/homebrew/bin/gpg",
            "/usr/local/bin/gpg",
            "/usr/bin/gpg",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).exists())
    }

    fn engine() -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/secrets.scm"))
            .unwrap();
        engine
            .eval("(define got '()) (define (got! x) (set! got (cons x got)))")
            .unwrap();
        engine
    }

    fn val(engine: &ScriptEngine, src: &str) -> String {
        engine.eval(src).unwrap().value.unwrap_or_default()
    }

    /// The (prompt, callback, on-cancel) of the ReadPasswd among `cmds`.
    fn passwd(cmds: &[EditorCommand]) -> (String, String, String) {
        cmds.iter()
            .find_map(|c| match c {
                EditorCommand::ReadPasswd {
                    prompt,
                    callback,
                    on_cancel,
                } => Some((
                    prompt.clone(),
                    callback.clone(),
                    on_cancel.clone().unwrap_or_default(),
                )),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected ReadPasswd in {cmds:?}"))
    }

    fn yes_no(cmds: &[EditorCommand]) -> (String, String) {
        cmds.iter()
            .find_map(|c| match c {
                EditorCommand::YesOrNo {
                    prompt,
                    callback,
                    sticky,
                } => {
                    assert!(sticky, "the save prompt must be sticky");
                    Some((prompt.clone(), callback.clone()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected YesOrNo in {cmds:?}"))
    }

    fn has_prompt(cmds: &[EditorCommand]) -> bool {
        cmds.iter().any(|c| {
            matches!(
                c,
                EditorCommand::ReadPasswd { .. } | EditorCommand::YesOrNo { .. }
            )
        })
    }

    #[test]
    fn secret_get_full_flow() {
        if !gpg_available() {
            eprintln!("gpg not installed; skipping");
            return;
        }
        let e = engine();
        let get = r#"(secret-get "db" "kyle" got! 'port "5432" 'on-cancel (lambda (_) (got! 'cancelled)))"#;

        // 1. No file yet: prompt for the password, offer to save, create the store.
        let (prompt, cb, _) = passwd(&e.eval(get).unwrap().commands);
        assert_eq!(prompt, "Password for kyle@db:5432: ");
        let (save_prompt, yes) = yes_no(&e.call_action(&cb, Some("pw one")));
        assert!(save_prompt.contains("kyle@db:5432"), "{save_prompt}");
        let (prompt, cb, _) = passwd(&e.call_action(&yes, Some("yes")));
        assert_eq!(prompt, "New authinfo.gpg passphrase: ");
        let (prompt, cb2, _) = passwd(&e.call_action(&cb, Some("master")));
        assert_eq!(prompt, "Confirm passphrase: ");
        assert!(!has_prompt(&e.call_action(&cb2, Some("master"))));
        assert_eq!(val(&e, "got"), r#"("pw one")"#);
        assert_eq!(val(&e, "(file-exists? (secrets-file))"), "#true");

        // 2. Unlocked: a hit answers with no prompt at all.
        assert!(!has_prompt(&e.eval(get).unwrap().commands));
        assert_eq!(val(&e, "(car got)"), r#""pw one""#);

        // 3. Locked: one passphrase prompt (a wrong one re-prompts), then two
        //    queued requests are both answered from the store.
        e.eval("(authinfo-lock) (set! got '())").unwrap();
        let first = e.eval(get).unwrap().commands;
        let (prompt, cb, _) = passwd(&first);
        assert_eq!(prompt, "authinfo.gpg passphrase: ");
        assert!(
            !has_prompt(&e.eval(get).unwrap().commands),
            "the second request waits its turn"
        );
        let (prompt, cb, _) = passwd(&e.call_action(&cb, Some("wrong")));
        assert_eq!(prompt, "(wrong passphrase) authinfo.gpg passphrase: ");
        assert!(!has_prompt(&e.call_action(&cb, Some("master"))));
        assert_eq!(val(&e, "got"), r#"("pw one" "pw one")"#);

        // 4. Forget (the service rejected it): deleted, so the next lookup
        //    prompts. Declining to save still delivers, and the password is
        //    remembered for the session — but not past `authinfo-lock`.
        e.eval(r#"(secret-forget "db" "kyle" "5432")"#).unwrap();
        assert_eq!(val(&e, r#"(secret-lookup "db" "kyle" "5432")"#), "#false");
        let (_, cb, _) = passwd(&e.eval(get).unwrap().commands);
        let (_, no) = yes_no(&e.call_action(&cb, Some("pw two")));
        assert!(!has_prompt(&e.call_action(&no, Some("no"))));
        assert_eq!(val(&e, "(car got)"), r#""pw two""#);
        assert!(!has_prompt(&e.eval(get).unwrap().commands), "session hit");
        e.eval("(authinfo-lock)").unwrap();
        assert_eq!(val(&e, r#"(secret-lookup "db" "kyle" "5432")"#), "#false");
        // Re-unlock: the forgotten entry is gone from the file, so it prompts.
        let (_, cb, _) = passwd(&e.eval(get).unwrap().commands);
        let (prompt, _, cancel) = passwd(&e.call_action(&cb, Some("master")));
        assert_eq!(prompt, "Password for kyle@db:5432: ");

        // 5. Cancelling a prompt calls on-cancel, and the queue moves on.
        e.call_action(&cancel, Some(""));
        assert_eq!(val(&e, "(car got)"), "cancelled");
        assert!(has_prompt(&e.eval(get).unwrap().commands));
    }
}

/// Secret references as option values: `(set-option k (secret-get h u))`
/// stores a reference (never the value), `get-option` resolves it only once
/// the store/session can answer, and `option-ensure` prompts on a miss. Uses
/// session memory (`authinfo-remember`), so no gpg is needed.
#[cfg(test)]
mod secret_options {
    use super::*;
    use crate::settings::OptionValue;

    fn engine() -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        engine.eval(include_str!("../../scheme/core.scm")).unwrap();
        engine
            .eval(include_str!("../../scheme/secrets.scm"))
            .unwrap();
        engine
    }

    fn val(engine: &ScriptEngine, src: &str) -> String {
        engine.eval(src).unwrap().value.unwrap_or_default()
    }

    fn secret_ref(host: &str, user: &str) -> OptionValue {
        OptionValue::Secret(crate::authinfo::SecretRef {
            host: host.into(),
            user: user.into(),
            port: String::new(),
        })
    }

    /// Options with psql-url as a reference and a store file that doesn't
    /// exist (so nothing asks for a master passphrase).
    fn with_ref(engine: &ScriptEngine) {
        let missing = std::env::temp_dir().join(format!("no-authinfo-{}.gpg", std::process::id()));
        let mut opts = HashMap::new();
        opts.insert("psql-url".to_string(), secret_ref("psql-url-1", "kyle"));
        opts.insert(
            "authinfo-file".to_string(),
            OptionValue::Str(missing.to_string_lossy().into_owned()),
        );
        engine.set_options(opts);
    }

    #[test]
    fn set_option_stores_the_reference_not_a_value() {
        let e = engine();
        let cmds = e
            .eval(r#"(set-option "psql-url" (secret-get "psql-url-1" "kyle"))"#)
            .unwrap()
            .commands;
        match cmds.as_slice() {
            [EditorCommand::SetOption(key, OptionValue::Secret(r))] => {
                assert_eq!(key, "psql-url");
                assert_eq!(
                    (r.host.as_str(), r.user.as_str(), r.port.as_str()),
                    ("psql-url-1", "kyle", "")
                );
            }
            other => panic!("expected SetOption(Secret), got {other:?}"),
        }
        // 'port rides along; the reference is its own opaque type.
        assert_eq!(
            val(&e, r#"(secret-ref? (secret-get "h" "u" 'port "5432"))"#),
            "#true"
        );
        assert_eq!(val(&e, r#"(secret-ref? "postgres://x")"#), "#false");
        assert_eq!(
            val(&e, r#"(secret-get "h" "u" 'port "5432")"#),
            "#<secret u@h:5432>"
        );
    }

    #[test]
    fn get_option_resolves_only_when_known() {
        let e = engine();
        with_ref(&e);
        assert_eq!(
            val(&e, r#"(get-option "psql-url")"#),
            "#false",
            "unknown → #f"
        );
        assert_eq!(
            val(&e, r#"(option-secret-ref "psql-url")"#),
            r#"("psql-url-1" "kyle" "")"#
        );
        assert_eq!(
            val(&e, r#"(option-secret-label "psql-url")"#),
            r#""kyle@psql-url-1""#
        );
        assert_eq!(
            val(&e, r#"(option-secret-label "authinfo-file")"#),
            "#false"
        );
        e.eval(r#"(authinfo-remember "psql-url-1" "kyle" "" "postgres://kyle:pw@db/app")"#)
            .unwrap();
        assert_eq!(
            val(&e, r#"(get-option "psql-url")"#),
            r#""postgres://kyle:pw@db/app""#
        );
        // Forgetting it (the server rejected it) makes it unknown again.
        assert_eq!(val(&e, r#"(option-secret-forget "psql-url")"#), "#true");
        assert_eq!(val(&e, r#"(get-option "psql-url")"#), "#false");
        assert_eq!(
            val(&e, r#"(option-secret-forget "authinfo-file")"#),
            "#false"
        );
    }

    /// A miss prompts (masked) for the value, naming the option; declining to
    /// save keeps it for the session and runs the gated thunk.
    #[test]
    fn option_ensure_prompts_then_runs() {
        let e = engine();
        with_ref(&e);
        e.eval("(define ran 0)").unwrap();
        let cmds = e
            .eval(r#"(option-ensure "psql-url" (lambda () (set! ran (+ ran 1))))"#)
            .unwrap()
            .commands;
        let (prompt, cb) = cmds
            .iter()
            .find_map(|c| match c {
                EditorCommand::ReadPasswd {
                    prompt, callback, ..
                } => Some((prompt.clone(), callback.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected ReadPasswd in {cmds:?}"));
        assert_eq!(prompt, "psql-url (kyle@psql-url-1): ");
        assert_eq!(val(&e, "ran"), "0");
        let save = e.call_action(&cb, Some("postgres://kyle:pw@db/app"));
        let no = save
            .iter()
            .find_map(|c| match c {
                EditorCommand::YesOrNo {
                    prompt, callback, ..
                } => {
                    assert_eq!(prompt, "Save kyle@psql-url-1 to authinfo.gpg?");
                    Some(callback.clone())
                }
                _ => None,
            })
            .expect("save offer");
        e.call_action(&no, Some("no"));
        assert_eq!(val(&e, "ran"), "1");
        assert_eq!(
            val(&e, r#"(get-option "psql-url")"#),
            r#""postgres://kyle:pw@db/app""#
        );
        // Known now: no prompt, runs immediately.
        let again = e
            .eval(r#"(option-ensure "psql-url" (lambda () (set! ran (+ ran 1))))"#)
            .unwrap()
            .commands;
        assert!(
            !again
                .iter()
                .any(|c| matches!(c, EditorCommand::ReadPasswd { .. }))
        );
        assert_eq!(val(&e, "ran"), "2");
        // A plain option never prompts.
        e.eval(r#"(option-ensure "authinfo-file" (lambda () (set! ran 10)))"#)
            .unwrap();
        assert_eq!(val(&e, "ran"), "10");
    }

    /// Answering "yes" to the save offer when the save can't happen (gpg not
    /// runnable, or the new-passphrase prompt cancelled) still keeps the value
    /// for the session and runs the gated thunk — `get-option` then answers.
    #[test]
    fn option_ensure_keeps_value_when_save_fails() {
        let e = engine();
        // As `with_ref`, plus a gpg that can't be run.
        let missing = std::env::temp_dir().join(format!("no-authinfo-{}.gpg", std::process::id()));
        let mut opts = HashMap::new();
        opts.insert("psql-url".to_string(), secret_ref("psql-url-1", "kyle"));
        opts.insert(
            "authinfo-file".to_string(),
            OptionValue::Str(missing.to_string_lossy().into_owned()),
        );
        opts.insert(
            "gpg-command".to_string(),
            OptionValue::Str("/nonexistent/gpg".into()),
        );
        e.set_options(opts);
        e.eval("(define ran 0)").unwrap();
        let ensure = r#"(option-ensure "psql-url" (lambda () (set! ran (+ ran 1))))"#;

        let passwd = |cmds: &[EditorCommand]| -> (String, String, String) {
            cmds.iter()
                .find_map(|c| match c {
                    EditorCommand::ReadPasswd {
                        prompt,
                        callback,
                        on_cancel,
                    } => Some((
                        prompt.clone(),
                        callback.clone(),
                        on_cancel.clone().unwrap_or_default(),
                    )),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("expected ReadPasswd in {cmds:?}"))
        };
        let yes_no = |cmds: &[EditorCommand]| -> String {
            cmds.iter()
                .find_map(|c| match c {
                    EditorCommand::YesOrNo { callback, .. } => Some(callback.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("expected YesOrNo in {cmds:?}"))
        };
        let message = |cmds: &[EditorCommand]| -> String {
            cmds.iter()
                .find_map(|c| match c {
                    EditorCommand::Message(m) => Some(m.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };

        // 1. Save fails (gpg can't run): kept for the session, hint shown.
        let (_, cb, _) = passwd(&e.eval(ensure).unwrap().commands);
        let yes = yes_no(&e.call_action(&cb, Some("postgres://one")));
        let (prompt, cb, _) = passwd(&e.call_action(&yes, Some("yes")));
        assert_eq!(prompt, "New authinfo.gpg passphrase: ");
        let (_, cb2, _) = passwd(&e.call_action(&cb, Some("master")));
        let done = e.call_action(&cb2, Some("master"));
        let m = message(&done);
        assert!(
            m.contains("save failed")
                && m.contains("can't run")
                && m.contains("gpg-command")
                && !m.contains("postgres://one"),
            "{m}"
        );
        assert_eq!(val(&e, "ran"), "1");
        assert_eq!(val(&e, r#"(get-option "psql-url")"#), r#""postgres://one""#);

        // 2. Cancelling the new-passphrase prompt: also kept for the session.
        e.eval(r#"(authinfo-lock) (option-secret-forget "psql-url")"#)
            .unwrap();
        assert_eq!(val(&e, r#"(get-option "psql-url")"#), "#false");
        let (_, cb, _) = passwd(&e.eval(ensure).unwrap().commands);
        let yes = yes_no(&e.call_action(&cb, Some("postgres://two")));
        let (_, _, cancel) = passwd(&e.call_action(&yes, Some("yes")));
        e.call_action(&cancel, Some(""));
        assert_eq!(val(&e, "ran"), "2");
        assert_eq!(val(&e, r#"(get-option "psql-url")"#), r#""postgres://two""#);

        // 3. An authinfo.gpg exists but gpg can't run: one passphrase try (no
        //    re-asking), then the value prompt, kept for the session without
        //    a save offer.
        let file = std::env::temp_dir().join(format!(
            "unreadable-authinfo-{}-{:?}.gpg",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&file, "not really encrypted").unwrap();
        e.eval("(authinfo-lock)").unwrap();
        let mut opts = HashMap::new();
        opts.insert("psql-url".to_string(), secret_ref("psql-url-1", "kyle"));
        opts.insert(
            "authinfo-file".to_string(),
            OptionValue::Str(file.to_string_lossy().into_owned()),
        );
        opts.insert(
            "gpg-command".to_string(),
            OptionValue::Str("/nonexistent/gpg".into()),
        );
        e.set_options(opts);
        let (prompt, cb, _) = passwd(&e.eval(ensure).unwrap().commands);
        assert_eq!(prompt, "authinfo.gpg passphrase: ");
        let after = e.call_action(&cb, Some("master"));
        assert!(message(&after).contains("gpg-command"), "{after:?}");
        let (prompt, cb, _) = passwd(&after);
        assert_eq!(prompt, "psql-url (kyle@psql-url-1): ");
        let done = e.call_action(&cb, Some("postgres://three"));
        assert!(
            !done
                .iter()
                .any(|c| matches!(c, EditorCommand::YesOrNo { .. })),
            "no save offer when gpg can't run: {done:?}"
        );
        std::fs::remove_file(&file).ok();
        assert_eq!(val(&e, "ran"), "3");
        assert_eq!(
            val(&e, r#"(get-option "psql-url")"#),
            r#""postgres://three""#
        );
    }

    #[test]
    fn url_decode() {
        let e = engine();
        assert_eq!(val(&e, r#"(url-decode "p%40ss%3Aw%2Fd")"#), r#""p@ss:w/d""#);
        assert_eq!(
            val(&e, r#"(url-decode "caf%C3%A9+%zz%4")"#),
            r#""café+%zz%4""#
        );
    }
}

/// Org ↔ plot: scheme babel blocks (org.scm) and org-plot/text (plotting-org.scm).
#[cfg(test)]
mod org_plot_tests {
    use super::*;

    fn engine_with(text: &str, point: usize) -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        engine.set_snapshot(EditorSnapshot {
            name: "notes.org".into(),
            text: Rope::from_str(text),
            point,
            line: text[..point].matches('\n').count(),
            ..Default::default()
        });
        engine
    }

    fn val(e: &ScriptEngine, q: &str) -> String {
        e.eval(q).unwrap().value.unwrap_or_default()
    }

    /// The text of the command's ReplaceRegion, i.e. the rewritten buffer.
    fn replaced(e: &ScriptEngine, q: &str) -> String {
        e.eval(q)
            .unwrap()
            .commands
            .into_iter()
            .find_map(|c| match c {
                EditorCommand::ReplaceRegion { text, .. } => Some(text),
                _ => None,
            })
            .expect("a ReplaceRegion")
    }

    #[test]
    fn scheme_block_values() {
        let e = engine_with("", 0);
        assert_eq!(val(&e, r#"(org--run-src "scheme" "(+ 1 2)")"#), r#""3""#);
        // A string is written as-is, a define (void) as nothing, and defines persist.
        assert_eq!(val(&e, r#"(org--run-src "scheme" "\"hi\"")"#), r#""hi""#);
        assert_eq!(
            val(&e, r#"(org--run-src "scheme" "(define zz-org 41)")"#),
            r#""""#
        );
        assert_eq!(
            val(&e, r#"(org--run-src "steel" "(+ zz-org 1)")"#),
            r#""42""#
        );
        assert_eq!(
            val(&e, r#"(org--run-src "scheme" "(list 1 \"a\")")"#),
            r#""(1 \"a\")""#
        );
        // Errors are caught and reported as the result.
        assert_eq!(
            val(
                &e,
                r#"(starts-with? (org--run-src "scheme" "(car '())") "Error: ")"#
            ),
            "#true"
        );
        // A plot comes back as its rows.
        assert_eq!(
            val(
                &e,
                r#"(org--styled-rows? (org--run-src "scheme" "(plot-bars (list (list \"a\" 1) (list \"b\" 2)))"))"#
            ),
            "#true"
        );
    }

    #[test]
    fn scheme_block_writes_results() {
        let text = "#+begin_src scheme\n(* 6 7)\n#+end_src\n\nafter\n";
        let e = engine_with(text, 20);
        assert_eq!(
            replaced(&e, "(org-babel-execute)"),
            "#+begin_src scheme\n(* 6 7)\n#+end_src\n#+RESULTS:\n: 42\n\nafter\n"
        );
    }

    #[test]
    fn chart_results_keep_their_colors() {
        // A written chart's rows are remembered, so the fontifier paints them.
        let e = engine_with("", 0);
        e.eval(
            r#"(define zz-p (plot-bars (list (list "a" 1) (list "b" 2))))
                  (org--remember-chart-rows zz-p)"#,
        )
        .unwrap();
        let text = format!(
            "#+RESULTS:\n: {}\n",
            val(&e, "(plot-row-text (car zz-p))").trim_matches('"')
        );
        e.set_snapshot(EditorSnapshot {
            name: "notes.org".into(),
            text: Rope::from_str(&text),
            ..Default::default()
        });
        let commands = e.eval("(org--fontify)").unwrap().commands;
        assert!(
            commands.iter().any(|c| matches!(
                c,
                EditorCommand::OverlayFace { start, face, .. }
                    if face.starts_with("plot-") && *start >= "#+RESULTS:\n: ".len()
            )),
            "no chart face painted: {commands:?}"
        );
        // An unknown results line is still plain.
        assert_eq!(val(&e, r#"(org--chart-line ": hello")"#), "#false");
    }

    #[test]
    fn src_block_body_is_painted_as_a_block() {
        // The body is literal code: org-block, never parsed for emphasis (the
        // `/repos/` in a path must not turn italic).
        let text = "#+BEGIN_SRC scheme\n(load \"~/repos/x.scm\")\n#+END_SRC\n";
        let e = engine_with("", 0);
        e.set_snapshot(EditorSnapshot {
            name: "notes.org".into(),
            text: Rope::from_str(text),
            ..Default::default()
        });
        let commands = e.eval("(org--fontify)").unwrap().commands;
        let body = "#+BEGIN_SRC scheme\n".len();
        let faces: Vec<_> = commands
            .iter()
            .filter_map(|c| match c {
                EditorCommand::OverlayFace { start, face, .. } => Some((*start, face.as_str())),
                _ => None,
            })
            .collect();
        assert!(faces.contains(&(body, "org-block")), "{faces:?}");
        assert!(
            !faces.iter().any(|(_, f)| *f == "org-italic"),
            "src body parsed for emphasis: {faces:?}"
        );
    }

    #[test]
    fn plot_options_parse() {
        let e = engine_with("", 0);
        assert_eq!(
            val(
                &e,
                r##"(org-plot--parse-options "#+PLOT: title:\"Two words\" ind:1 deps:(2 3) type:bars")"##
            ),
            r#"(("title" "Two words") ("ind" "1") ("deps" "2 3") ("type" "bars"))"#
        );
        assert_eq!(val(&e, r#"(org-plot--parse-deps "2 3")"#), "(1 2)");
    }

    const TABLE: &str = "#+PLOT: title:\"Load\" type:bars\n| day | req | err |\n|-----+-----+-----|\n| mon | 10  | 1   |\n| tue | 25  | 3   |\n\nafter\n";

    #[test]
    fn plot_table_as_bars() {
        let e = engine_with(TABLE, 40);
        let out = replaced(&e, "(org-plot/text)");
        assert!(
            out.starts_with(&TABLE[..TABLE.find("\n\nafter").unwrap()]),
            "{out}"
        );
        assert!(out.contains("|\n#+RESULTS:\n: Load\n"), "{out}");
        // Two deps → a titled chart per column, with each row's label.
        for want in [": req\n", ": err\n", ": mon ", ": tue "] {
            assert!(out.contains(want), "missing {want:?}: {out}");
        }
        assert!(out.ends_with("\n\nafter\n"), "{out}");
    }

    #[test]
    fn plot_table_as_lines() {
        let text = "#+PLOT: deps:(2) width:30 height:5\n| x | y |\n|---+---|\n| 1 | 2 |\n| 2 | 4 |\n| 3 | 1 |\n";
        let e = engine_with(text, 40);
        let out = replaced(&e, "(org-plot/text)");
        let results: Vec<&str> = out
            .lines()
            .skip_while(|l| *l != "#+RESULTS:")
            .skip(1)
            .collect();
        assert!(results.len() >= 5, "{out}");
        assert!(
            results.iter().all(|l| l.starts_with(": ") || l.is_empty()),
            "{out}"
        );
        // Not in a table → no rewrite.
        let e = engine_with("plain\n", 0);
        assert!(
            e.eval("(org-plot/text)")
                .unwrap()
                .commands
                .iter()
                .all(|c| !matches!(c, EditorCommand::ReplaceRegion { .. }))
        );
    }

    #[test]
    fn plot_key_is_bound() {
        let e = engine_with("", 0);
        assert_eq!(
            val(&e, r#"(key-binding "org-mode" "ctrl-c \" g")"#),
            r#""org-plot/text""#
        );
    }
}

#[cfg(test)]
mod titlebar_menu_lib {
    use super::*;

    /// The last `SetTitlebarMenu` an eval emitted, as `(title, items, commands)`.
    fn published(engine: &ScriptEngine, q: &str) -> (String, Vec<String>, Vec<String>) {
        let commands = engine.eval(q).unwrap().commands;
        match commands.into_iter().last() {
            Some(EditorCommand::SetTitlebarMenu(m)) => (m.title, m.items, m.commands),
            other => panic!("expected SetTitlebarMenu, got {other:?}"),
        }
    }

    /// Rows keep insertion order; re-adding a label rebinds it in place; a
    /// symbol command passes through by name and a lambda gets a stub name.
    #[test]
    fn add_publishes_rows_in_order() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "titlebar-menu");
        published(
            &engine,
            r#"(titlebar-menu-add "Dashboards" "Postgres" 'psql-dashboard)"#,
        );
        let (title, items, commands) = published(
            &engine,
            r#"(titlebar-menu-add "Dashboards" "Kubernetes" "k8s-dashboard")"#,
        );
        assert_eq!(title, "Dashboards");
        assert_eq!(items, ["Postgres", "Kubernetes"]);
        assert_eq!(commands, ["psql-dashboard", "k8s-dashboard"]);

        let (_, items, commands) = published(
            &engine,
            r#"(titlebar-menu-add "Dashboards" "Postgres" (lambda () 1))"#,
        );
        assert_eq!(items, ["Postgres", "Kubernetes"]);
        assert!(commands[0].starts_with("hook-fn-"), "{commands:?}");
    }

    /// Removing the last row publishes an empty menu (which drops it from the
    /// title bar); clearing does the same in one step.
    #[test]
    fn remove_and_clear_empty_the_menu() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "titlebar-menu");
        engine
            .eval(r#"(titlebar-menu-add "Tools" "A" 'a) (titlebar-menu-add "Tools" "B" 'b)"#)
            .unwrap();
        let (_, items, _) = published(&engine, r#"(titlebar-menu-remove "Tools" "A")"#);
        assert_eq!(items, ["B"]);
        let (_, items, commands) = published(&engine, r#"(titlebar-menu-clear "Tools")"#);
        assert!(items.is_empty() && commands.is_empty());
    }

    /// A hidden menu publishes no rows however its owner refreshes it, and
    /// showing it again restores them.
    #[test]
    fn hide_survives_refresh_until_shown() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "titlebar-menu");
        engine
            .eval(r#"(titlebar-menu-add "Tools" "A" 'a)"#)
            .unwrap();
        let (_, items, _) = published(&engine, r#"(titlebar-menu-hide "Tools")"#);
        assert!(items.is_empty());
        let (_, items, _) = published(
            &engine,
            r#"(titlebar-menu-set! "Tools" (list (cons "B" "")) "")"#,
        );
        assert!(items.is_empty());
        let (_, items, _) = published(&engine, r#"(titlebar-menu-show "Tools")"#);
        assert_eq!(items, ["B"]);
    }

    /// `titlebar-menu-configure!` sets the heading and the on-select function
    /// (by name) that rows without a command of their own fall back to.
    #[test]
    fn configure_sets_section_and_on_select() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "titlebar-menu");
        engine
            .eval(r#"(titlebar-menu-configure! "Projects" 'section "Recent Projects" 'on-select 'perspective-select)"#)
            .unwrap();
        let commands = engine
            .eval(r#"(titlebar-menu-set! "Projects" (list (cons "macros" "")) "macros")"#)
            .unwrap()
            .commands;
        match commands.as_slice() {
            [EditorCommand::SetTitlebarMenu(m)] => {
                assert_eq!(m.items, ["macros"]);
                assert_eq!(m.commands, [""]);
                assert_eq!(m.active, "macros");
                assert_eq!(m.section, "Recent Projects");
                assert_eq!(m.callback, "perspective-select");
            }
            other => panic!("expected one SetTitlebarMenu, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod bookmarks_lib {
    use super::*;

    fn engine() -> ScriptEngine {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "bookmarks");
        engine
    }

    fn v(engine: &ScriptEngine, q: &str) -> String {
        engine
            .eval(q)
            .unwrap_or_else(|e| panic!("{q}: {e}"))
            .value
            .unwrap_or_default()
    }

    #[test]
    fn reader_parses_strings_numbers_dotted_pairs_and_comments() {
        let engine = engine();
        let src = concat!(
            ";; a comment the reader must skip\n",
            "(\"work\"\n",
            "  (filename . \"~/org/Work.org\")\n",
            "  (position . 123)\n",
            "  (line . 4)\n",
            "  (column . 2)\n",
            "  (front-context-string . \"* MiView\")\n",
            "  (rear-context-string . \"\"))\n"
        );
        engine
            .eval(&format!("(define got (bm--read-all {:?}))", src))
            .unwrap();
        assert_eq!(
            v(
                &engine,
                r#"(equal? got '(("work"
                    ("filename" . "~/org/Work.org")
                    ("position" . 123)
                    ("line" . 4)
                    ("column" . 2)
                    ("front-context-string" . "* MiView")
                    ("rear-context-string" . ""))))"#
            ),
            "#true"
        );
    }

    #[test]
    fn escape_round_trips_quotes_backslashes_and_control_chars() {
        let engine = engine();
        engine
            .eval(r#"(define s (string-append "a\"b\\c" "\n" "x\ty\rd"))"#)
            .unwrap();
        // The escaped form is one line with no raw quote/tab/newline…
        assert_eq!(
            v(&engine, r#"(string-contains? (bm-escape s) "\n")"#),
            "#false"
        );
        // …and parse(escape(s)) == s.
        engine
            .eval(r#"(define rt (car (bm--read-all (string-append "\"" (bm-escape s) "\""))))"#)
            .unwrap();
        assert_eq!(v(&engine, "(equal? rt s)"), "#true");
    }

    #[test]
    fn store_writes_file_and_load_restores() {
        let engine = engine();
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        engine
            .eval(r#"(bookmark-store "home" "~/org/Home.org" 0 0 0 "" "")"#)
            .unwrap();
        // Newest set wins on a name collision.
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work2.org" 5 0 0 "" "")"#)
            .unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "2");
        let file = v(&engine, "(bookmarks-file)");
        let file = file.trim_matches('"').to_string();
        let text = std::fs::read_to_string(&file).expect("bookmark file written");
        assert!(text.contains(";;;; Macros Bookmark Format Version 1"));
        assert!(text.contains(r#""~/org/Work2.org""#));
        assert!(text.contains(r#""~/org/Home.org""#));

        // A fresh load from disk restores everything, with the file's comments
        // and header skipped.
        engine.eval("(set! *bookmarks* '())").unwrap();
        engine.eval("(bm-load)").unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "2");
        assert_eq!(
            v(
                &engine,
                r#"(alist-ref (alist-ref *bookmarks* "work") "position")"#
            ),
            "5"
        );
        assert_eq!(
            v(
                &engine,
                r#"(alist-ref (alist-ref *bookmarks* "home") "filename")"#
            ),
            r#""~/org/Home.org""#
        );
    }

    #[test]
    fn candidates_select_by_exact_string_even_with_a_separator_in_the_name() {
        let engine = engine();
        // A name containing the display separator itself must round-trip.
        engine
            .eval(r#"(bookmark-store "a — b" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        assert_eq!(v(&engine, "(length (bm-candidates))"), "1");
        // Selection matches the whole candidate string against every
        // bookmark's rendered line (Emacs-style: no parsing of the display);
        // a jump actually queues the file open.
        let out = engine
            .eval(r#"(bm-jump-selected (car (bm-candidates)))"#)
            .unwrap();
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::OpenFile(p) if p == "~/org/Work.org")),
            "no open queued for the known selection: {:?}",
            out.commands
        );
        // An unknown selection says so instead of jumping.
        let out = engine
            .eval(r#"(bm-jump-selected "nope — ~/x.org")"#)
            .unwrap();
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::Message(m) if m.contains("No such bookmark"))),
            "no message for the unknown selection: {:?}",
            out.commands
        );
        // Delete removes both the a-list entry and (after the rewrite) the file's.
        engine.eval(r#"(bm-delete-named! "a — b")"#).unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "0");
        engine.eval("(bm-load)").unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "0");
    }

    #[test]
    fn rename_keeps_the_entry_fields() {
        let engine = engine();
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        // Drive the extracted rename mutation directly (the command itself
        // goes through interactive read-string).
        engine.eval(r#"(bm-rename! "work" "wurk")"#).unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "1");
        // The renamed entry is still ("name" (field . value) …): jumping and
        // serializing read its fields, so a flattened entry fails here loudly.
        assert_eq!(
            v(
                &engine,
                r#"(bm-field (alist-ref *bookmarks* "wurk") "filename")"#
            ),
            r#""~/org/Work.org""#
        );
        assert_eq!(
            v(
                &engine,
                r#"(bm-field (alist-ref *bookmarks* "wurk") "front-context-string")"#
            ),
            r#""* MiView""#
        );
        // Renaming onto an existing name is refused (it would delete that
        // entry) — and silently at that; the fields are untouched.
        engine
            .eval(r#"(bookmark-store "other" "~/org/Other.org" 1 0 0 "" "")"#)
            .unwrap();
        engine.eval(r#"(bm-rename! "wurk" "other")"#).unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "2");
        assert_eq!(
            v(
                &engine,
                r#"(bm-field (alist-ref *bookmarks* "wurk") "filename")"#
            ),
            r#""~/org/Work.org""#
        );
        // …and the file on disk round-trips through a fresh load.
        engine.eval("(set! *bookmarks* '())").unwrap();
        engine.eval("(bm-load)").unwrap();
        assert_eq!(v(&engine, "(length *bookmarks*)"), "2");
        assert_eq!(
            v(
                &engine,
                r#"(bm-field (alist-ref *bookmarks* "wurk") "position")"#
            ),
            "123"
        );
    }

    #[test]
    fn commands_registered_for_mx() {
        let engine = engine();
        for c in [
            "bookmark-set",
            "bookmark-jump",
            "helm-bookmarks",
            "bookmark-delete",
            "bookmark-rename",
        ] {
            assert_eq!(
                engine
                    .eval(&format!(
                        r#"(if (member "{c}" *interactive-commands*) "y" "n")"#
                    ))
                    .unwrap()
                    .value
                    .unwrap(),
                "\"y\"",
                "{c} not registered"
            );
        }
    }

    #[test]
    fn org_capture_sets_last_stored_bookmark() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "org");
        // Append after two complete lines: the bookmark lands on line 2, byte 4.
        engine
            .eval(r#"(org--bookmark-last-stored "~/org/inbox.org" "a\nb\n" "" "* TODO x")"#)
            .unwrap();
        let e = v(
            &engine,
            r#"(alist-ref *bookmarks* "org-capture-last-stored")"#,
        );
        assert!(e.contains("2"), "line field: {e}");
        assert!(
            v(
                &engine,
                r#"(equal? (alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "position") 4)"#
            ) == "#true",
            "position field: {e}"
        );

        // A file not ending in a newline gets the joining "\n" first, so the
        // headline lands one line BELOW the newline count of `existing` alone.
        engine
            .eval(r#"(org--bookmark-last-stored "~/org/inbox.org" "a" "\n" "* TODO y")"#)
            .unwrap();
        let e = v(
            &engine,
            r#"(alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "line")"#,
        );
        assert_eq!(e, "1", "line with joining newline: {e}");
        assert_eq!(
            v(
                &engine,
                r#"(equal? (alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "position") 2)"#
            ),
            "#true",
            "position with joining newline"
        );
    }

    /// Play the editor's side of `edit-file-text`: check the capture asked for
    /// `path`'s text, hand the callback `existing`, and return what it answered
    /// with `set-file-text`.
    fn answer_file_edit(engine: &ScriptEngine, cmds: &[EditorCommand], existing: &str) -> String {
        let (path, callback) = cmds
            .iter()
            .find_map(|c| match c {
                EditorCommand::EditFileText { path, callback } => Some((path, callback)),
                _ => None,
            })
            .expect("capture should ask for the target's text");
        let answer = engine
            .eval(&format!(
                "({callback} {} {})",
                crate::script::steel_quote(path),
                crate::script::steel_quote(existing)
            ))
            .unwrap();
        answer
            .commands
            .into_iter()
            .find_map(|c| match c {
                EditorCommand::SetFileText { path: p, text } if &p == path => Some(text),
                _ => None,
            })
            .expect("the callback should answer with set-file-text")
    }

    #[test]
    fn org_capture_under_headline_lands_on_the_inserted_item() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "org");
        // The capture never touches the file itself: it asks the editor for
        // the target's text (an open buffer's, if any) and answers with the
        // new text, so the path only has to pass org--resolve-path.
        let path = "/tmp/capture.org";
        let commit = |item: &str| {
            engine
                .eval(&format!(
                    r#"(org--capture-commit (list "t" "T" (list "file+headline" "{path}" "Inbox")) "{item}")"#
                ))
                .unwrap()
                .commands
        };
        // A "file+headline" capture files the text as a child of the headline,
        // AFTER its subtree end — the bookmark must land on the inserted item,
        // not on the parent headline.
        let text = answer_file_edit(&engine, &commit("* TODO x"), "** Inbox\n* Other\n");
        // "** Inbox" (line 0) has no body, so the item is inserted at line 1:
        // position = bytes of the one preceding line + its newline.
        let e = v(
            &engine,
            r#"(alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "line")"#,
        );
        assert_eq!(e, "1", "headline-capture line: {e}");
        assert_eq!(
            v(
                &engine,
                r#"(equal? (alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "position") 9)"#
            ),
            "#true",
            "headline-capture position"
        );
        assert!(
            text.contains("*** TODO x"),
            "item not filed as a child: {text}"
        );

        // A second capture under the same headline lands below the first item.
        let text = answer_file_edit(&engine, &commit("* TODO y"), &text);
        let e = v(
            &engine,
            r#"(alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "line")"#,
        );
        assert_eq!(e, "2", "second capture line: {e}");
        assert_eq!(
            v(
                &engine,
                r#"(equal? (alist-ref (cdr (alist-ref *bookmarks* "org-capture-last-stored")) "position") 20)"#
            ),
            "#true",
            "second capture position"
        );
        assert_eq!(text, "** Inbox\n*** TODO x\n*** TODO y\n* Other\n");

        // With no such headline the item is appended to the end.
        let text = answer_file_edit(
            &engine,
            &engine
                .eval(&format!(
                    r#"(org--capture-commit (list "t" "T" (list "file+headline" "{path}" "Nope")) "* TODO z")"#
                ))
                .unwrap()
                .commands,
            "* Other",
        );
        assert_eq!(text, "* Other\n* TODO z\n");
    }

    #[test]
    fn org_simple_capture_appends_through_the_editor() {
        let (engine, _rx) = ScriptEngine::new();
        load_with_deps(&engine, "org");
        engine
            .eval(r#"(org-set-capture-file "/tmp/inbox.org")"#)
            .unwrap();
        let cmds = engine
            .eval(r#"(org--capture-finish "  buy milk ")"#)
            .unwrap()
            .commands;
        // The capture is never written straight to disk (where a buffer
        // visiting the file would lose it on its next save).
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::SetFileText { .. })),
            "capture must wait for the target's current text: {cmds:?}"
        );
        let text = answer_file_edit(&engine, &cmds, "* DONE a");
        assert_eq!(text, "* DONE a\n* TODO buy milk\n");
    }

    #[test]
    fn annotation_round_trips_through_file_store_and_re_set() {
        let engine = engine();
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        assert_eq!(v(&engine, r#"(bookmark-annotation "work")"#), "#false");

        engine
            .eval(r#"(bookmark-set-annotation! "work" "the chapter I keep losing")"#)
            .unwrap();
        assert_eq!(
            v(&engine, r#"(bookmark-annotation "work")"#),
            "\"the chapter I keep losing\""
        );

        // The file on disk carries the field, and a fresh load restores it.
        let file = v(&engine, "(bookmarks-file)");
        let file = file.trim_matches('"').to_string();
        let text = std::fs::read_to_string(&file).expect("bookmark file written");
        assert!(text.contains(r#"(annotation . "the chapter I keep losing")"#));
        engine.eval("(set! *bookmarks* '())").unwrap();
        engine.eval("(bm-load)").unwrap();
        assert_eq!(
            v(&engine, r#"(bookmark-annotation "work")"#),
            "\"the chapter I keep losing\""
        );

        // Re-setting the bookmark (new position) keeps the annotation — it
        // describes the target, not the point.
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 999 8 0 "" "")"#)
            .unwrap();
        assert_eq!(
            v(&engine, r#"(bookmark-annotation "work")"#),
            "\"the chapter I keep losing\""
        );
        assert_eq!(
            v(
                &engine,
                r#"(bm-field (alist-ref *bookmarks* "work") "position")"#
            ),
            "999"
        );

        // Empty input clears the annotation, and the field drops off disk.
        engine
            .eval(r#"(bookmark-set-annotation! "work" "")"#)
            .unwrap();
        assert_eq!(v(&engine, r#"(bookmark-annotation "work")"#), "#false");
        let file = v(&engine, "(bookmarks-file)");
        let text = std::fs::read_to_string(file.trim_matches('"')).unwrap();
        assert!(!text.contains("annotation"), "cleared field still on disk");
    }

    #[test]
    fn annotated_bookmarks_render_as_helm_marginalia_pairs() {
        let engine = engine();
        engine
            .eval(r#"(bookmark-store "plain" "~/org/P.org" 0 0 0 "" "")"#)
            .unwrap();
        engine
            .eval(r#"(bookmark-store "noted" "~/org/N.org" 0 0 0 "" "")"#)
            .unwrap();
        engine
            .eval(r#"(bookmark-set-annotation! "noted" "review me")"#)
            .unwrap();

        // The annotated one is a (candidate annotation) pair; the plain one a
        // bare string. Only the candidate string is matched back on selection.
        let sel = v(
            &engine,
            r#"(bm-candidate (cons "noted" (alist-ref *bookmarks* "noted")))"#,
        );
        assert!(sel.contains("noted — ~/org/N.org"), "candidate line: {sel}");
        assert_eq!(
            v(&engine, r#"(length (filter pair? (bm-candidates)))"#),
            "1",
            "exactly one pair: {:?}",
            v(&engine, "(bm-candidates)")
        );
        assert_eq!(
            v(
                &engine,
                r#"(equal? (cadr (car (filter pair? (bm-candidates)))) "review me")"#
            ),
            "#true"
        );
        // Selection still matches the displayed line, annotation or not — and
        // a jump actually queues the file open.
        let out = engine
            .eval(r#"(bm-jump-selected "noted — ~/org/N.org")"#)
            .unwrap();
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::OpenFile(p) if p == "~/org/N.org")),
            "no open queued for the annotated selection: {:?}",
            out.commands
        );
    }

    #[test]
    fn save_load_and_after_jump_hooks_dispatch_by_name() {
        let engine = engine();
        // bm-save fires the save hook on every rewrite (saving is automatic).
        let out = engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RunHook(h) if h == "bookmark-save-hook")),
            "no save hook in {:?}",
            out.commands
        );
        // And bm-load the load hook (fires here even with no file yet).
        let out = engine.eval("(bm-load)").unwrap();
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RunHook(h) if h == "bookmark-load-hook")),
            "no load hook in {:?}",
            out.commands
        );

        // A jump must NOT fire the after-jump hook straight away — the goto
        // parks until the file loads. The hook waits behind the pending visit.
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        let out = engine.eval(r#"(bm-jump "work")"#).unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RunHook(h) if h == "bookmark-after-jump-hook")),
            "fired before landing: {:?}",
            out.commands
        );
        // Once the visit has landed (the goto step clears *visit-pending* and
        // flags *visit-result* 'landed), the poller fires the hook exactly once.
        engine
            .eval("(set! *visit-pending* #f) (set! *visit-result* 'landed)")
            .unwrap();
        let out = engine.eval("(bm--after-jump-step)").unwrap();
        assert_eq!(
            out.commands
                .iter()
                .filter(
                    |c| matches!(c, EditorCommand::RunHook(h) if h == "bookmark-after-jump-hook")
                )
                .count(),
            1,
            "no after-jump hook once landed: {:?}",
            out.commands
        );
        // …and not a second time: the pending flag is spent.
        let out = engine.eval("(bm--after-jump-step)").unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RunHook(_))),
            "hook re-fired: {:?}",
            out.commands
        );

        // Two jumps issued before the first visit lands each stay owed a
        // fire: the counter merges NOTHING — the landing pays every jump.
        // (Each goto-when-visiting re-arms *visit-pending*, so it's cleared
        // again here — the landing the harness simulates.)
        engine.eval(r#"(bm-jump "work")"#).unwrap();
        engine.eval(r#"(bm-jump "work")"#).unwrap();
        engine
            .eval("(set! *visit-pending* #f) (set! *visit-result* 'landed)")
            .unwrap();
        let out = engine.eval("(bm--after-jump-step)").unwrap();
        assert_eq!(
            out.commands
                .iter()
                .filter(
                    |c| matches!(c, EditorCommand::RunHook(h) if h == "bookmark-after-jump-hook")
                )
                .count(),
            2,
            "consecutive jumps should each fire: {:?}",
            out.commands
        );
        // A jump whose visit TIMED OUT (the file never became current) pays
        // nothing: the hook must not fire on a failed jump.
        engine.eval(r#"(bm-jump "work")"#).unwrap();
        engine
            .eval("(set! *visit-pending* #f) (set! *visit-result* 'timed-out)")
            .unwrap();
        let out = engine.eval("(bm--after-jump-step)").unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RunHook(_))),
            "hook fired on a timed-out jump: {:?}",
            out.commands
        );
        assert_eq!(v(&engine, "*bm-jumps-owed*"), "0", "owed not drained");
        engine
            .eval("(set! *visit-pending* #f) (set! *visit-result* 'landed)")
            .unwrap();
        let out = engine.eval("(bm--after-jump-step)").unwrap();
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, EditorCommand::RunHook(_))),
            "counter not drained: {:?}",
            out.commands
        );
    }

    #[test]
    fn annotation_edit_command_registered_and_prompts_prefilled() {
        let engine = engine();
        assert_eq!(
            engine
                .eval(r#"(if (member "bookmark-edit-annotation" *interactive-commands*) "y" "n")"#)
                .unwrap()
                .value
                .unwrap(),
            "\"y\""
        );
        engine
            .eval(r#"(bookmark-store "work" "~/org/Work.org" 123 4 2 "* MiView" "")"#)
            .unwrap();
        engine
            .eval(r#"(bookmark-set-annotation! "work" "chapter 3")"#)
            .unwrap();
        // The extracted prompt helper carries the current annotation as the
        // initial minibuffer text; empty input clears it (the ! setter).
        engine.eval(r#"(bm-annotation-prompt "")"#).unwrap(); // no bookmark named ""
        assert_eq!(
            v(&engine, r#"(bookmark-annotation "work")"#),
            "\"chapter 3\""
        );
        engine.eval(r#"(bm-annotation-prompt "no-such")"#).unwrap();
        assert_eq!(
            v(&engine, r#"(bookmark-annotation "work")"#),
            "\"chapter 3\""
        );
    }
}
