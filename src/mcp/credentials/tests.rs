//! None of these touch the process environment: `${VAR}` is looked up in
//! a map handed to the function under test, so they cannot race each
//! other or anything else.

use super::*;

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
}

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn ex(template: &str, vars: &[(&str, &str)]) -> Result<String, ExpandError> {
    expand(template, &env(vars))
}

// ----------------------------------------------------------- expansion

#[test]
fn a_set_variable_is_expanded_anywhere_in_the_value() {
    let vars = [("TOK", "s3cret")];
    assert_eq!(ex("Bearer ${TOK}", &vars).unwrap(), "Bearer s3cret");
    assert_eq!(ex("${TOK}", &vars).unwrap(), "s3cret");
    assert_eq!(ex("a${TOK}b", &vars).unwrap(), "as3cretb");
    assert_eq!(ex("no references", &vars).unwrap(), "no references");
    assert_eq!(ex("", &vars).unwrap(), "");
}

#[test]
fn several_references_in_one_value_each_expand() {
    let vars = [("A", "1"), ("B", "2"), ("_C_3", "3")];
    assert_eq!(ex("${A}-${B}-${_C_3}-${A}", &vars).unwrap(), "1-2-3-1");
}

#[test]
fn an_unset_variable_is_an_error_naming_the_variable_only() {
    let e = ex("Bearer ${MISSING}", &[("OTHER", "x")]).unwrap_err();
    assert_eq!(e, ExpandError::Unset("MISSING".to_string()));
    assert_eq!(e.to_string(), "environment variable MISSING is not set");
    // The first unset one is reported, even when others are fine.
    assert_eq!(
        ex("${OK}${NOPE}", &[("OK", "v")]).unwrap_err(),
        ExpandError::Unset("NOPE".to_string())
    );
}

/// The recorded decision: unset is a mistake, empty is a statement.
#[test]
fn an_empty_variable_is_allowed_and_expands_to_nothing() {
    assert_eq!(ex("Bearer ${EMPTY}", &[("EMPTY", "")]).unwrap(), "Bearer ");
    assert_eq!(ex("${EMPTY}", &[("EMPTY", "")]).unwrap(), "");
}

#[test]
fn a_bare_dollar_name_is_not_expanded() {
    let vars = [("HOME", "/root"), ("TOK", "s3cret")];
    assert_eq!(ex("$HOME", &vars).unwrap(), "$HOME");
    assert_eq!(ex("Bearer $TOK", &vars).unwrap(), "Bearer $TOK");
    // Dollars in a pasted token are plain text, whatever follows them.
    assert_eq!(ex("pa$$word$", &vars).unwrap(), "pa$$word$");
    assert_eq!(ex("$", &vars).unwrap(), "$");
    assert_eq!(ex("a$ {TOK}", &vars).unwrap(), "a$ {TOK}");
}

#[test]
fn dollar_dollar_brace_is_the_one_escape() {
    let vars = [("X", "val")];
    assert_eq!(ex("$${X}", &vars).unwrap(), "${X}");
    assert_eq!(ex("a$${X}b", &vars).unwrap(), "a${X}b");
    // The escape does not need the reference to be well formed, or set.
    assert_eq!(ex("$${NOT_SET}", &vars).unwrap(), "${NOT_SET}");
    assert_eq!(ex("$${", &vars).unwrap(), "${");
    // Left to right: `$` then the escape. A literal `$` before a real
    // reference is not expressible, and is documented as such.
    assert_eq!(ex("$$${X}", &vars).unwrap(), "$${X}");
    // `$$` that is not followed by `{` is untouched.
    assert_eq!(ex("$$X", &vars).unwrap(), "$$X");
    // An escape and a reference side by side.
    assert_eq!(ex("$${X}${X}", &vars).unwrap(), "${X}val");
}

#[test]
fn malformed_references_are_errors_never_passed_through() {
    let vars = [("A", "1"), ("B", "2")];
    for unterminated in ["${A", "x ${", "Bearer ${A and more", "${A}${B"] {
        assert_eq!(ex(unterminated, &vars).unwrap_err(), ExpandError::Unterminated, "{unterminated}");
    }
    for bad in ["${}", "${A B}", "${A-B}", "${A:-d}", "${1A}", "${A${B}}", "${ A }", "${é}", "${A}${}"] {
        assert_eq!(ex(bad, &vars).unwrap_err(), ExpandError::BadName, "{bad}");
    }
}

#[test]
fn an_error_never_quotes_the_template_text_it_rejected() {
    for e in [ExpandError::Unterminated, ExpandError::BadName] {
        let shown = e.to_string();
        assert!(!shown.contains("s3cret"), "{shown}");
    }
    // Not even a variable's name is echoed unless it is a valid name.
    assert_eq!(ex("${s3cret hunter2}", &[]).unwrap_err(), ExpandError::BadName);
}

#[test]
fn an_expanded_value_is_not_expanded_again() {
    let vars = [("OUTER", "${INNER}"), ("INNER", "leaked")];
    assert_eq!(ex("${OUTER}", &vars).unwrap(), "${INNER}");
}

// ------------------------------------------------------------- masking

#[test]
fn redact_url_masks_userinfo_and_query_and_leaves_the_rest() {
    for (input, want) in [
        ("http://127.0.0.1:8931/mcp", "http://127.0.0.1:8931/mcp"),
        ("https://u:p@h.example/mcp", "https://***@h.example/mcp"),
        ("https://h.example/mcp?token=abc#frag", "https://h.example/mcp?***"),
        ("https://h.example/mcp#only-a-fragment", "https://h.example/mcp?***"),
        // Userinfo with no path, and with a query straight after the host.
        ("https://u:p@h.example", "https://***@h.example"),
        ("https://u:p@h.example?k=v", "https://***@h.example?***"),
        // An `@` after the authority is part of the path, not userinfo.
        ("https://h.example/@user/x", "https://h.example/@user/x"),
        // The userinfo trick: what is shown is the real host.
        ("http://127.0.0.1@evil.com/x", "http://***@evil.com/x"),
        // Several `@`: the last one closes the userinfo.
        ("https://a@b:c@h.example/x", "https://***@h.example/x"),
        // A password holding `?` or `#` unescaped still goes.
        ("https://u:p?w#d@h.example/x", "https://***@h.example/x"),
        // Not a URL at all: the query is still masked, nothing else.
        ("not a url?token=abc", "not a url?***"),
        // Templates are shown as written.
        ("https://${HOST}/${PATH}", "https://${HOST}/${PATH}"),
        ("https://h.example/mcp?key=${KEY}", "https://h.example/mcp?***"),
    ] {
        assert_eq!(redact_url(input), want, "{input}");
    }
}

#[test]
fn redact_arg_masks_credential_keys_and_urls_and_nothing_else() {
    for (input, want) in [
        ("-y", "-y"),
        ("@scope/server-postgres", "@scope/server-postgres"),
        ("--root=/srv", "--root=/srv"),
        ("GITHUB_TOKEN=ghp_abc123", "GITHUB_TOKEN=***"),
        ("--api-key=abc", "--api-key=***"),
        ("DB_PASSWORD=hunter2", "DB_PASSWORD=***"),
        ("CLIENT_SECRET=x", "CLIENT_SECRET=***"),
        ("postgresql://admin:hunter2@db.internal:5432/app", "postgresql://***@db.internal:5432/app"),
        // A URL argument loses its query too, which the old helper kept.
        ("https://h.example/x?api_key=abc", "https://h.example/x?***"),
        // A flag whose value is a URL.
        ("--url=https://u:p@h.example/x", "--url=https://***@h.example/x"),
        ("--url=https://h.example/x?t=1", "--url=https://h.example/x?***"),
        // A credential key wins over the URL in its value.
        ("API_KEY=https://u:p@h/x", "API_KEY=***"),
    ] {
        assert_eq!(redact_arg(input), want, "{input}");
    }
}

#[test]
fn credential_looking_names() {
    for yes in [
        "token", "Token", "access_token", "api_key", "apikey", "X-Api-Key", "key", "secret",
        "client_secret", "password", "passwd", "auth", "Authorization", "sig", "X-Amz-Sig",
        "signature", "X-Amz-Signature", "bearer", "cookie", "jwt", "credentials", "accessToken",
    ] {
        assert!(is_credential_name(yes), "{yes}");
    }
    for no in ["page", "q", "limit", "design", "signal", "name", "sort", "id"] {
        assert!(!is_credential_name(no), "{no}");
    }
}

#[test]
fn scrub_masks_every_secret_longest_first() {
    let secrets = vec!["abcd".to_string(), "Bearer abcd".to_string()];
    assert_eq!(scrub("rejected: Bearer abcd (abcd)", &secrets), "rejected: *** (***)");
    assert_eq!(scrub("nothing to hide", &secrets), "nothing to hide");
    assert_eq!(scrub("anything", &[]), "anything");
}

// ------------------------------------------------------------ loopback

fn loopback(u: &str) -> bool {
    is_loopback(&Url::parse(u).unwrap())
}

#[test]
fn which_hosts_are_loopback() {
    for yes in [
        "http://localhost/",
        "http://LOCALHOST:8931/mcp",
        "http://localhost./",
        "http://127.0.0.1/",
        "http://127.8.9.10:1/",
        "http://127.1/",
        "http://2130706433/",
        "http://[::1]:8080/",
        "http://[::ffff:127.0.0.1]/",
        "http://[0:0:0:0:0:0:0:1]/",
        "https://127.0.0.1/",
    ] {
        assert!(loopback(yes), "{yes}");
    }
    for no in [
        "http://localhost.evil.com/",
        "http://evil.com/localhost",
        "http://notlocalhost/",
        "http://127.0.0.1.nip.io/",
        "http://127.0.0.1@evil.com/",
        "http://localhost@evil.com/",
        "http://localhost:80@evil.com/",
        "http://evil.com#@127.0.0.1/",
        "http://[::ffff:10.0.0.1]/",
        "http://[::2]/",
        "http://0.0.0.0/",
        "http://128.0.0.1/",
        "http://10.0.0.5/",
        "http://192.168.1.2:8931/",
        "http://playwright.localhost/",
        "http://[2001:db8::1]/",
        "https://mcp.example.com/",
    ] {
        assert!(!loopback(no), "{no}");
    }
}

// ------------------------------------------------- the cleartext refusal

fn resolve(url: &str, hdrs: &[(&str, &str)], vars: &[(&str, &str)]) -> Result<ResolvedHttp, String> {
    resolve_http("gw", url, &headers(hdrs), &env(vars))
}

const AUTH: (&str, &str) = ("Authorization", "Bearer s3cret");

#[test]
fn cleartext_credentials_to_a_remote_host_are_refused() {
    for url in [
        "http://mcp.example.com/mcp",
        "http://10.0.0.5:8931/mcp",
        "http://192.168.1.2/mcp",
        "http://[2001:db8::1]/mcp",
        "http://0.0.0.0/mcp",
        // Hostnames that only look local.
        "http://localhost.evil.com/mcp",
        "http://127.0.0.1.nip.io/mcp",
        "http://notlocalhost/mcp",
        "http://[::ffff:10.0.0.1]/mcp",
        "http://playwright.localhost/mcp",
        "HTTP://MCP.EXAMPLE.COM/mcp",
    ] {
        let Err(e) = resolve(url, &[AUTH], &[]) else { panic!("{url} was accepted") };
        assert!(e.contains("\"gw\""), "{e}");
        assert!(e.contains("cleartext"), "{e}");
        assert!(e.contains("https://"), "suggests the fix: {e}");
        assert!(e.contains("headers"), "says what carried the credential: {e}");
        assert!(!e.contains("s3cret") && !e.contains("Bearer"), "{e}");
    }
}

#[test]
fn the_refusal_names_the_host_and_never_the_url_or_query() {
    let Err(e) = resolve("http://mcp.example.com/p4th?api_key=hunter2", &[], &[]) else {
        panic!("accepted")
    };
    assert!(e.contains("mcp.example.com"), "{e}");
    assert!(!e.contains("hunter2") && !e.contains("p4th"), "{e}");
}

#[test]
fn userinfo_tricks_cannot_make_a_remote_host_look_local() {
    for url in [
        "http://127.0.0.1@evil.com/mcp",
        "http://localhost@evil.com/mcp",
        "http://127.0.0.1:80@evil.com/mcp",
        "http://user:pass@evil.com/mcp",
    ] {
        // No headers at all: the userinfo is itself the credential.
        let Err(e) = resolve(url, &[], &[]) else { panic!("{url} was accepted") };
        assert!(e.contains("evil.com") && e.contains("userinfo"), "{e}");
        assert!(!e.contains("pass"), "{e}");
    }
    // The same URL over https is the user's choice to make.
    assert!(resolve("https://127.0.0.1@evil.com/mcp", &[], &[]).is_ok());
}

#[test]
fn a_credential_looking_query_parameter_counts_as_a_credential() {
    for url in [
        "http://mcp.example.com/mcp?token=abc",
        "http://mcp.example.com/mcp?x=1&API_KEY=abc",
        "http://mcp.example.com/mcp?sig=abc",
        "http://mcp.example.com/mcp?X-Amz-Signature=abc",
    ] {
        let Err(e) = resolve(url, &[], &[]) else { panic!("{url} was accepted") };
        assert!(e.contains("query parameter"), "{e}");
        assert!(!e.contains("abc"), "{e}");
    }
}

#[test]
fn cleartext_without_credentials_to_a_remote_host_is_allowed() {
    // Nothing secret to leak: not this check's business.
    for url in [
        "http://mcp.example.com/mcp",
        "http://mcp.example.com/mcp?page=2&design=1",
        "http://10.0.0.5:8931/mcp",
    ] {
        assert!(resolve(url, &[], &[]).is_ok(), "{url}");
    }
}

#[test]
fn loopback_http_with_credentials_is_fine_and_https_is_always_fine() {
    for url in [
        "http://localhost:8931/mcp",
        "http://127.0.0.1:8931/mcp",
        "http://127.9.9.9/mcp",
        "http://[::1]:8931/mcp",
        "http://[::ffff:127.0.0.1]/mcp",
        "https://mcp.example.com/mcp",
        "https://u:p@mcp.example.com/mcp?token=abc",
        "HTTPS://MCP.EXAMPLE.COM/mcp",
    ] {
        assert!(resolve(url, &[AUTH], &[]).is_ok(), "{url}");
    }
}

#[test]
fn the_refusal_judges_the_expanded_url() {
    // The host comes from the environment, so the scheme and host that
    // matter are the ones after expansion.
    let vars = [("HOST", "evil.com"), ("LOCAL", "localhost"), ("SCHEME", "http")];
    assert!(resolve("http://${HOST}/mcp", &[AUTH], &vars).is_err());
    assert!(resolve("http://${LOCAL}:8931/mcp", &[AUTH], &vars).is_ok());
    assert!(resolve("${SCHEME}://${HOST}/mcp", &[AUTH], &vars).is_err());
    assert!(resolve("https://${HOST}/mcp", &[AUTH], &vars).is_ok());
    // A variable cannot smuggle userinfo past the check either.
    let vars = [("CRED", "127.0.0.1@evil.com")];
    assert!(resolve("http://${CRED}/mcp", &[], &vars).is_err());
}

// ----------------------------------------------------- resolving, generally

#[test]
fn resolving_expands_the_url_and_every_header_value() {
    let vars = [("HOST", "mcp.example.com"), ("TOK", "s3cret"), ("TEAM", "blue")];
    let r = resolve(
        "https://${HOST}/mcp?team=${TEAM}",
        &[("Authorization", "Bearer ${TOK}"), ("X-Team", "${TEAM}"), ("X-Plain", "v")],
        &vars,
    )
    .unwrap();
    assert_eq!(r.url.as_str(), "https://mcp.example.com/mcp?team=blue");
    let got: BTreeMap<String, String> = r
        .headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    assert_eq!(got["authorization"], "Bearer s3cret");
    assert_eq!(got["x-team"], "blue");
    assert_eq!(got["x-plain"], "v");
    assert!(r.headers.iter().all(|(_, v)| v.is_sensitive()));
    // What a server might echo back is what gets scrubbed.
    assert!(r.secrets.contains(&"Bearer s3cret".to_string()));
    assert!(r.secrets.contains(&"s3cret".to_string()));
}

#[test]
fn header_names_are_not_expanded() {
    let Err(e) = resolve("https://h/mcp", &[("X-${NAME}", "v")], &[("NAME", "A")]) else {
        panic!("accepted")
    };
    assert!(e.contains("not a valid header name"), "{e}");
}

#[test]
fn an_unset_variable_names_the_server_and_the_variable_never_a_value() {
    let vars = [("OTHER", "other-secret-value")];
    let Err(e) = resolve("https://h/mcp", &[("Authorization", "Bearer ${GW_TOKEN}")], &vars)
    else {
        panic!("accepted")
    };
    assert!(e.contains("\"gw\"") && e.contains("GW_TOKEN") && e.contains("Authorization"), "{e}");
    assert!(e.contains("skipped"), "{e}");
    assert!(!e.contains("other-secret-value") && !e.contains("Bearer"), "{e}");

    let Err(e) = resolve("https://${GW_HOST}/mcp", &[], &vars) else { panic!("accepted") };
    assert!(e.contains("\"gw\"") && e.contains("GW_HOST") && e.contains("url"), "{e}");
}

#[test]
fn a_value_invalid_after_expansion_is_diagnosed_without_echoing_it() {
    // A trailing newline, as from `export TOK="$(cat token-file)"`.
    let vars = [("TOK", "s3cr\net")];
    let Err(e) = resolve("https://h/mcp", &[("Authorization", "Bearer ${TOK}")], &vars) else {
        panic!("accepted")
    };
    assert!(e.contains("Authorization") && e.contains("not valid"), "{e}");
    assert!(!e.contains("s3cr"), "{e}");
    // Other control characters too.
    let vars = [("TOK", "a\u{1}b"), ("DEL", "a\u{7f}b")];
    for t in ["${TOK}", "${DEL}"] {
        assert!(resolve("https://h/mcp", &[("X", t)], &vars).is_err(), "{t}");
    }
    // An invalid name, and a template that never expands.
    let Err(e) = resolve("https://h/mcp", &[("Bad Name", "v")], &[]) else { panic!("accepted") };
    assert!(e.contains("Bad Name"), "{e}");
    assert!(resolve("https://h/mcp", &[("X", "${UNTERMINATED")], &[]).is_err());
}

#[test]
fn an_expansion_that_is_not_a_url_is_diagnosed_without_echoing_it() {
    let vars = [("BASE", "not a url with s3cret")];
    for url in ["${BASE}", "${BASE}/mcp"] {
        let Err(e) = resolve(url, &[], &vars) else { panic!("{url} accepted") };
        assert!(!e.contains("s3cret"), "{e}");
    }
    let Err(e) = resolve("example.com/mcp", &[], &[]) else { panic!("accepted") };
    assert!(e.contains("http://"), "{e}");
    let Err(e) = resolve("ftp://example.com/mcp", &[], &[]) else { panic!("accepted") };
    assert!(e.contains("ftp"), "{e}");
}

#[test]
fn empty_expansion_is_a_valid_header_value() {
    let r = resolve("https://h/mcp", &[("X-Opt", "${NONE}")], &[("NONE", "")]).unwrap();
    assert_eq!(r.headers[0].1.to_str().unwrap(), "");
}

#[test]
fn a_literal_dollar_brace_never_reaches_the_wire_unless_escaped() {
    // Unset: refused, not sent as the literal text.
    assert!(resolve("https://h/mcp", &[("Authorization", "${TOKEN}")], &[]).is_err());
    // Escaped: the user asked for it.
    let r = resolve("https://h/mcp", &[("X-T", "$${TOKEN}")], &[]).unwrap();
    assert_eq!(r.headers[0].1.to_str().unwrap(), "${TOKEN}");
}

// ------------------------------------------------- referenced variables

#[test]
fn referenced_names_lists_every_reference_in_order() {
    assert_eq!(referenced_names("Bearer ${TOKEN}"), vec!["TOKEN"]);
    assert_eq!(referenced_names("${A}-${B}/${A}"), vec!["A", "B", "A"]);
    assert_eq!(referenced_names("https://${HOST}:${PORT}/mcp"), vec!["HOST", "PORT"]);
    assert!(referenced_names("plain").is_empty());
}

#[test]
fn referenced_names_follows_the_expansion_grammar_and_never_fails() {
    // `$NAME` is not a reference; `$${` is the escape.
    assert!(referenced_names("$TOKEN").is_empty());
    assert!(referenced_names("$${TOKEN}").is_empty());
    // What `expand` rejects names nothing; a missing `}` ends the scan.
    assert!(referenced_names("${}").is_empty());
    assert!(referenced_names("${A:-b}").is_empty());
    assert!(referenced_names("${UNTERMINATED").is_empty());
    assert_eq!(referenced_names("${bad name} ${GOOD}"), vec!["GOOD"]);
}

#[test]
fn the_recorded_set_holds_names_only_and_only_grows() {
    record_referenced_env(["CRED_UNIT_TEST_ONE".to_string()]);
    record_referenced_env(["CRED_UNIT_TEST_TWO".to_string(), "CRED_UNIT_TEST_ONE".to_string()]);
    let all = referenced_env_names();
    assert_eq!(all.iter().filter(|n| *n == "CRED_UNIT_TEST_ONE").count(), 1);
    assert!(all.iter().any(|n| n == "CRED_UNIT_TEST_TWO"));
}
