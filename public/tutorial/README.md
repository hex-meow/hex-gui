# Tutorial media

Drop the getting-started screenshots / screen-recordings referenced by the
in-app tutorial here. Vite serves everything under `public/` at the site root,
so a file at `public/tutorial/01-connect.png` is loaded as `/tutorial/01-connect.png`.

## Landing-page "Getting started" guide

Referenced by the `HOME_SLIDES` array in `src/components/Tutorial.tsx`:

- `01-connect.png`  — choosing the CAN interface and pressing Connect
- `02-select.png`   — picking a motor from the sidebar
- `03-drive.mp4`    — driving a motor and watching the chart

## Per-app tutorials

Every tool has its own tutorial, opened by the **Tutorial / 使用教程** button in
that app's header. Each starts with blank placeholder steps that already point
at a per-app subfolder, so filling one in is just dropping a screenshot:

- `control/01.png`, `control/02.png`, …   — Motor Control
- `changeId/01.png` …                     — Change Node ID
- `zero/01.png` …                         — Position Preset (Zero)
- `hopea3/01.png` …                       — HopeA3 base
- `smartknob/01.png` …                    — SmartKnob
- `zenoh/01.png` …                        — Base (Zenoh)
- `arm/01.png` …                          — Arm (Zenoh)
- `canalyzer/01.png` …                    — CAN Analyzer

After adding a screenshot, replace that step's placeholder body text in the
`placeholderSlides` output — or write proper slides for the tool in the
`TUTORIALS` map in `src/components/Tutorial.tsx`. Change the step `count` there
to add or remove steps. To use a video instead, drop an `.mp4` and switch the
slide's `media.type` to `"video"`.

Slides whose media file is missing fall back to a "(screenshot / video goes here)"
caption automatically, so every tutorial works before you add real media.
Recommended image size: ~1200×840 (the slide area is 640px wide × 280px tall).
