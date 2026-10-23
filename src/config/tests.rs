use super::*;

const MINIMAL: &str = r#"
server:
  public_url: https://media.example.com
database:
  url: postgres://fc:fc@db/fc
auth:
  oidc:
    issuer: https://id.example.com/realms/home
    client_id: ferrocine
libraries:
  - id: movies
    name: Movies
    kind: movies
    paths: [/media/movies]
"#;

#[test]
fn minimal_configuration_gets_defaults() {
    let c = from_str(MINIMAL).unwrap();
    assert_eq!(c.server.listen.port(), 8096);
    assert_eq!(c.libraries.len(), 1);
    assert_eq!(c.libraries[0].kind, LibraryKind::Movies);
    assert_eq!(c.transcoding.hw_accel, HwAccel::Auto);
    let o = c.auth.oidc.unwrap();
    assert_eq!(o.scopes, ["openid", "profile", "email"]);
    assert_eq!(o.groups_claim, "groups");
    assert_eq!(c.auth.session_idle, None);
}

#[test]
fn unknown_keys_are_reported_with_their_path() {
    let text = MINIMAL.replace("kind: movies", "kind: movies\n    colour: red");
    let e = from_str(&text).unwrap_err().to_string();
    assert!(e.contains("libraries[0]") && e.contains("colour"), "{e}");
}

#[test]
fn semantic_errors_are_all_listed() {
    let text = r#"
server: { public_url: "https://media.example.com/ui" }
database: { url: "sqlite://x.db" }
libraries:
  - { id: "bad id", name: A, kind: movies, paths: [relative] }
  - { id: shows, name: B, kind: shows, paths: [/media] }
  - { id: shows, name: C, kind: shows, paths: [/media/tv] }
"#;
    let e = from_str(text).unwrap_err().to_string();
    for needle in [
        "public_url",
        "SQLite",
        "auth.oidc is required",
        "\"bad id\"",
        "must be absolute",
        "duplicate id",
        "overlaps",
    ] {
        assert!(e.contains(needle), "missing {needle:?} in {e}");
    }
}

#[test]
fn environment_references_are_typed() {
    let text = MINIMAL.replace(
        "client_id: ferrocine",
        "client_id: ferrocine\n    client_secret: ${FERROCINE_TEST_UNSET_SECRET:-1234}",
    );
    let c = from_str(&text).unwrap();
    // A numeric secret stays a string.
    assert_eq!(c.auth.oidc.unwrap().client_secret.as_deref(), Some("1234"));
}

#[test]
fn http_issuer_only_on_localhost() {
    let text = MINIMAL.replace("https://id.example.com", "http://id.example.com");
    assert!(from_str(&text).is_err());
    let text = MINIMAL.replace("https://id.example.com", "http://localhost:8080");
    assert!(from_str(&text).is_ok());
}

#[test]
fn sections_merge_across_files_but_not_twice() {
    let dir = std::env::temp_dir().join(format!("fc-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (head, libs) = MINIMAL.split_once("libraries:").unwrap();
    std::fs::write(dir.join("00-base.yaml"), head).unwrap();
    std::fs::write(dir.join("10-libraries.yml"), format!("libraries:{libs}")).unwrap();
    std::fs::write(
        dir.join("20-more.yaml"),
        "libraries:\n  - { id: tv, name: TV, kind: shows, paths: [/media/tv] }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".hidden.yaml"), "garbage: [").unwrap();
    let c = load(&dir).unwrap();
    assert_eq!(c.libraries.len(), 2);
    std::fs::write(dir.join("30-dup.yaml"), "server: { public_url: https://x.example }\n").unwrap();
    let e = load(&dir).unwrap_err().to_string();
    assert!(e.contains("section `server` is defined in both"), "{e}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_example_is_valid() {
    let text = include_str!("../../examples/config.yaml");
    // Only the database password and the client secret are required: the
    // providers' keys default to empty (off).
    let required = regex::Regex::new(r"\$\{[A-Z_]+\}").unwrap();
    let names: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| required.find_iter(l).map(|m| m.as_str()))
        .collect();
    assert_eq!(names, ["${POSTGRES_PASSWORD}", "${OIDC_CLIENT_SECRET}"]);
    // Defaults resolved here: the test must not depend on the environment.
    let optional = regex::Regex::new(r"\$\{[A-Z_]+:-([^}]*)\}").unwrap();
    let defaults = optional.replace_all(text, |c: &regex::Captures| match &c[1] {
        "" => "''".to_owned(),
        d => d.to_owned(),
    });
    let c = from_str(&required.replace_all(&defaults, "x")).unwrap();
    assert!(c.metadata.tmdb.is_none() && c.metadata.anidb.is_none() && c.subtitles.opensubtitles.is_none());
    // With every key set, every provider is on.
    let all = regex::Regex::new(r"\$\{[A-Z_]+(:-[^}]*)?\}").unwrap();
    let keyed = all.replace_all(text, |c: &regex::Captures| match c.get(1).map(|d| &d.as_str()[2..]) {
        Some(d) if !d.is_empty() => d.to_owned(),
        _ => "x".to_owned(),
    });
    let c = from_str(&keyed).unwrap();
    assert!(c.metadata.tvdb.is_some() && c.metadata.anidb.is_some() && c.subtitles.opensubtitles.is_some());
    assert!(c.libraries.iter().any(|l| l.anime));
}

#[test]
fn an_empty_key_switches_a_provider_off() {
    let text = format!(
        "{MINIMAL}metadata:\n  tmdb: {{ api_key: '' }}\n  tvdb: {{ api_key: k, pin: '' }}\n  omdb: {{ api_key: '' }}\n\
         subtitles:\n  opensubtitles: {{ api_key: '', username: '', password: '' }}\n  languages: [fr]\n"
    );
    let c = from_str(&text).unwrap();
    assert!(c.metadata.tmdb.is_none() && c.metadata.omdb.is_none() && c.subtitles.opensubtitles.is_none());
    assert_eq!(c.metadata.tvdb.unwrap().pin, None);
    let text = format!("{MINIMAL}subtitles:\n  opensubtitles: {{ api_key: k, username: '', password: p }}\n");
    assert!(
        from_str(&text)
            .unwrap_err()
            .to_string()
            .contains("username and password are required")
    );
}

#[test]
fn original_title_languages() {
    let text = format!(
        "{MINIMAL}  - {{ id: anime, name: Anime, kind: shows, paths: [/media/anime], prefer_original_title: [] }}\n\
         metadata:\n  prefer_original_title: [ja, ko]\n"
    );
    let c = from_str(&text).unwrap();
    assert_eq!(c.libraries[0].original_title_languages(&c.metadata), ["ja", "ko"]);
    assert!(c.libraries[1].original_title_languages(&c.metadata).is_empty());
    let text = format!("{MINIMAL}metadata:\n  prefer_original_title: ['*', japanese]\n");
    let e = from_str(&text).unwrap_err().to_string();
    assert!(
        e.contains("metadata.prefer_original_title: unknown language \"japanese\""),
        "{e}"
    );
}
