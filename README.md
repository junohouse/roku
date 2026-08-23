# Roku

Roku TVs and Roku players, over ECP on port 8060.

One package, two drivers. A Roku TV is genuinely two things — a streamer and a display — so it
carries both a `media_player` and a `tv` proxy: "watch Netflix" goes to one, volume to the
other. A player is the same box without the screen, and the same control code drives both, so
shipping them apart would mean maintaining one implementation twice.

| Driver | Proxies | Leads with |
| --- | --- | --- |
| `roku.tv` | `tv`, `media_player` | `tv` — it is a television, with a streamer in it |
| `roku.player` | `media_player` | `media_player` |

Which of the two you get is not a question anybody is asked. Both are `roku:ecp` on port 8060
and nothing on the wire separates them, so only `roku.player` declares discovery; setup reads
`is-tv` out of `/query/device-info` before it offers anything and hands back whichever id the
box says it is.

## Setup

Discovered over SSDP (`roku:ecp`). Nothing to pair — ECP is unauthenticated on the LAN, which
is Roku's design, not ours.

## Building

```bash
cargo build --release
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release. To work on this against a local core checkout,
uncomment the `[patch]` block in `Cargo.toml`.
