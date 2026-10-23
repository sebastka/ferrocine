# Deployment

Ferrocine runs as two containers and a database:

- **`ghcr.io/sebastka/ferrocine`**, the server: a static Rust binary plus
  Jellyfin's ffmpeg build with current drivers for AMD, Intel and NVIDIA GPUs,
  on Docker Hardened Images (Debian trixie). It runs as user 65532 with a read-only root
  filesystem and no capability.
- **`ghcr.io/sebastka/ferrocine-ui`**, the web app on nginx. It is the entry
  point: it serves the app and proxies `/api/`, `/auth/`, `/healthz` and
  `/readyz` to the server, so both share one origin (the session cookie is
  first-party). Put your TLS reverse proxy or ingress in front of it.
- **PostgreSQL or MariaDB**, holding state, metadata and artwork: back it up.

Ferrocine needs, besides its configuration:

| Path | Access | Holds |
|---|---|---|
| `/etc/ferrocine` | read-only | `*.yaml` configuration |
| your libraries, e.g. `/media` | read-only | media files, `.nfo`, artwork, subtitles |
| `transcoding.temp_dir` | read-write, ephemeral | HLS segments of running playbacks (a few GB at most) |
| `/tmp` | read-write, small | Mesa's shader cache |

## Docker Compose

[`deploy/docker-compose.yaml`](../deploy/docker-compose.yaml) runs the UI,
Ferrocine and PostgreSQL, with the configuration in
[`deploy/config/`](../deploy/config):

```sh
cd deploy
$EDITOR config/00-ferrocine.yaml config/10-libraries.yaml
POSTGRES_PASSWORD=$(openssl rand -hex 16) OIDC_CLIENT_SECRET=… MEDIA_DIR=/srv/media \
  RENDER_GID=$(getent group render | cut -d: -f3) docker compose up -d
docker compose logs -f ferrocine
```

Apply configuration changes with `docker kill -s HUP ferrocine` (or the UI's
admin page, or `POST /api/v1/reload`). Libraries, access lists, sign-in,
metadata and scanner settings apply at once; changes to the listener, public
URL, database or transcoding are reported in the log and apply at the next
restart.

## Kubernetes

[`deploy/kubernetes.yaml`](../deploy/kubernetes.yaml) has both Deployments
and Services with the restricted pod security profile. Provide the Secret,
the ConfigMap, a read-only PersistentVolumeClaim with the media, and an
Ingress to `ferrocine-ui`. ConfigMap updates are not picked up by themselves:
call `POST /api/v1/reload`, or let a reloader restart the pod. Keep one replica of Ferrocine: playback sessions
live in the process (DECISIONS.md, D14).

## OpenID Connect provider

Register a confidential client (or a public one: leave `client_secret` out;
PKCE is always used):

- **Redirect URI:** `<public_url>/auth/callback`
- **Post-logout redirect URI:** `<public_url>/`
- **Scopes:** `openid profile email`
- **Groups:** put the user's groups or roles in the ID token or in userinfo,
  and point `groups_claim` at them. Then `users` lists who may sign in,
  `admins` who administers, and `libraries[].access` who sees what.

Keycloak: a client with "Client authentication" on and the standard flow;
for realm roles use `groups_claim: realm_access.roles`, or add a "Group
Membership" mapper (claim `groups`, full path off). Authentik and Authelia
send `groups` by default.

The issuer must be reachable from Ferrocine (discovery, keys, token and
userinfo endpoints) and from browsers. `http://` issuers are accepted on
localhost only.

## Hardware acceleration

The image serves AMD, Intel and NVIDIA GPUs. With `transcoding.hw_accel: auto`
(the default), Ferrocine tries VA-API, Quick Sync, then NVENC at start, each
with a short test encode, and logs the result (`ffmpeg ready … accel="vaapi"`);
`GET /api/v1/system` shows it too. Each transcode then tries the full GPU
pipeline, then software decoding with GPU encoding, then software.

**Intel and AMD (VA-API, Quick Sync).** Pass the render node and add the group
that owns it on the host; the image's user is not root:

```yaml
devices:
  - /dev/dri:/dev/dri
group_add:
  - "303"   # getent group render | cut -d: -f3
```

```sh
docker run --device /dev/dri --group-add "$(getent group render | cut -d: -f3)" …
```

On Kubernetes, use the Intel or AMD GPU device plugin and add the group in
`securityContext.supplementalGroups`. With several GPUs, set
`transcoding.device` (e.g. `/dev/dri/renderD129`).

Check from inside the container:

```sh
docker exec ferrocine /usr/lib/jellyfin-ffmpeg/vainfo --display drm --device /dev/dri/renderD128
docker exec ferrocine ffmpeg -hide_banner -init_hw_device vaapi=va:/dev/dri/renderD128 \
  -f lavfi -i testsrc2 -frames:v 30 -vf format=nv12,hwupload -c:v h264_vaapi -f null -
```

Tested on an AMD Radeon RX 6900 XT: 4K HEVC 10-bit transcoded to 1080p H.264
at 7× real time, entirely on the GPU. Quick Sync is implemented but has not
been run on Intel hardware yet.

**NVIDIA (NVENC).** Install the NVIDIA container toolkit on the host and give
the container the GPU. The image sets `NVIDIA_DRIVER_CAPABILITIES` to
`compute,video,utility`, so the toolkit mounts the driver's CUDA, NVENC/NVDEC
and management libraries, which the image's ffmpeg loads:

```yaml
deploy:
  resources:
    reservations:
      devices:
        - driver: nvidia
          count: 1
          capabilities: [gpu]
```

```sh
docker run --gpus all …
```

On Kubernetes, request `nvidia.com/gpu: 1` with the NVIDIA device plugin.
NVENC is implemented but has not been run on NVIDIA hardware yet; if its test
encode fails, the log says why and transcodes use software.

## Database

PostgreSQL (tested with 18) or MariaDB (tested with 11.8 and 13.0). Create a
database and a user owning it; migrations run at start (or with
`ferrocine migrate`). Several instances may share a database; they queue on
migrations and never scan the same library together.

## Probes and metrics

- `/healthz`: the process answers. The image's `HEALTHCHECK` runs
  `ferrocine healthcheck`.
- `/readyz`: the database answers.
- `/metrics`: OpenMetrics; protect it with `server.metrics_token` or keep it
  off the public proxy (the UI does not forward it).
