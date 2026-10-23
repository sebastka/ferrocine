# Ferrocine

> [!NOTE]
> This project was created with [Anthropic Claude](https://www.anthropic.com/claude).

A self-hosted media server written in Rust, an alternative to Jellyfin and
Emby for people who keep their infrastructure in code. This repository is the
server; the web app is [ferrocine-ui](https://github.com/sebastka/ferrocine-ui).

- **Read-only libraries.** Ferrocine never writes next to your media. Metadata
  and artwork (resized) live in the database, so there is nothing else to back up.
- **PostgreSQL or MariaDB.** No SQLite. Instances sharing a database share
  everything and never scan the same library at once.
- **Configuration as code.** Server, sign-in, transcoding and libraries are
  YAML files with `${ENV}` and `${file:…}` secrets, reloaded on `SIGHUP`. The
  database holds state and metadata only.
- **OpenID Connect sign-in** (authorization code with PKCE). Groups from your
  provider decide who may sign in, who administers, and who sees which library.
- **Hardware transcoding.** VA-API (Intel, AMD), Quick Sync and NVENC (NVIDIA)
  in one image, each proven with a test encode at start, with a fallback to
  software per stream.
  Direct play when the browser can, remux when only the container is wrong,
  transcode otherwise; seeking anywhere, audio track choice and subtitles.
- **Metadata** from `.nfo` files, local artwork, and optionally TMDB; frames
  extracted when there is no image at all. Jellyfin, Kodi and Plex naming
  conventions work as they are.
- **Hardened image** on Docker Hardened Images: non-root, read-only root,
  no capability.

## Quick start

```sh
cd deploy
$EDITOR config/00-ferrocine.yaml config/10-libraries.yaml   # public URL, provider, libraries
POSTGRES_PASSWORD=$(openssl rand -hex 16) OIDC_CLIENT_SECRET=… MEDIA_DIR=/srv/media \
  RENDER_GID=$(getent group render | cut -d: -f3) docker compose up -d
```

Then open the UI on port 8080 (behind your TLS proxy). Register the client at
your provider with the redirect URI `https://<public_url>/auth/callback`; see
[deployment](docs/deployment.md).

To try it locally without a provider: `docker compose -f dev/compose.yaml up -d --build`
starts a mock provider, PostgreSQL, Ferrocine and the UI on http://localhost:18080
with the media in `dev/media/{movies,shows}`.

```yaml
libraries:
  - id: movies
    name: Movies
    kind: movies
    paths: [/media/movies]
  - id: kids
    name: Kids
    kind: movies
    paths: [/media/kids]
    access: [kids, parents]   # groups from the OIDC provider
```

## Documentation

- [Configuration reference](examples/config.yaml)
- [Libraries: naming and metadata](docs/libraries.md)
- [Deployment](docs/deployment.md): Docker, Kubernetes, hardware acceleration, OpenID Connect
- [API](docs/api.md)
- [Design decisions](DECISIONS.md)

## Command line

```
ferrocine [--config PATH] [--log-format text|json] [run|validate|migrate|healthcheck]
```

| Variable | Purpose |
|---|---|
| `FERROCINE_CONFIG` | Configuration file or directory (default `/etc/ferrocine`) |
| `FERROCINE_LOG` | Log filter, e.g. `info,ferrocine=debug` |
| `FERROCINE_LOG_FORMAT` | `text` or `json` |

## Development

The toolchain is Rust 1.96, the version shipped by the hardened build image.
Tests that need media call `ffmpeg` and are skipped without it.

```sh
cargo test                       # unit tests; database tests are skipped
FERROCINE_TEST_POSTGRES=postgres://fc:fc@localhost:5432/fc \
FERROCINE_TEST_MARIADB=mariadb://fc:fc@localhost:3306/fc \
  cargo test                     # with the PostgreSQL, MariaDB and end-to-end tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
yamllint --strict .
cargo deny check
docker build -t ferrocine:dev .  # needs `docker login dhi.io`
```

## License

[MIT](LICENSE)
