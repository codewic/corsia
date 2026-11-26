# Corsia management page

`corsia serve` serves this page at `http://127.0.0.1:7715/` (the daemon's port), built into the binary with `include_str!` and `include_bytes!` in `src/api.rs`, so a machine needs nothing else installed.

| File | What it is |
| --- | --- |
| `index.html` | The whole page: markup, styles and script, no framework and no build step |
| `geist.woff2` | Geist (variable), self-hosted, served at `/ui/geist.woff2` |

The page reads the daemon's live status (`/api/events`), the models in `~/models` (`/api/models`), and the coding-assistant endpoint's finished requests and live text (`/api/agent/events`, `/api/agent/live`, passed through by the daemon). It switches models with `POST /api/models/use` (needs an `X-Corsia` header), which runs `local-agent.sh use NAME` as its own unit. The Chat button opens llama.cpp's chat page on the llama-server port; it needs llama-server built with its interface (`-DLLAMA_BUILD_UI=ON`).

Art, all hand-built and inline in `index.html`:

| Asset | Where |
| --- | --- |
| Corsia mark (three lanes, the middle one lit and pointed, a packet moving along it on hover) | Header, favicon (on its white plate). Source: `brand/marks/corsia-light.svg`, `corsia-dark.svg` |
| Lanes spot illustration (the lit lane drifting between two empty lanes) | Empty states: nothing being written, no models in the folder |

Status glyphs (check, warning, error) are small inline SVGs; the search and chat icons are line icons inside buttons.
