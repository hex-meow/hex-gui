# Tutorial media

Drop the getting-started screenshots / screen-recordings referenced by the
in-app tutorial here. Vite serves everything under `public/` at the site root,
so a file at `public/tutorial/01-connect.png` is loaded as `/tutorial/01-connect.png`.

Files currently referenced by `src/components/Tutorial.tsx`:

- `01-connect.png`  — choosing the CAN interface and pressing Connect
- `02-select.png`   — picking a motor from the sidebar
- `03-drive.mp4`    — driving a motor and watching the chart

Slides whose media file is missing fall back to a "(screenshot / video goes here)"
caption automatically, so the tutorial still works before you add real media.
Recommended image size: ~1200×840 (the slide area is 640px wide × 280px tall).
Edit the `SLIDES` array in `src/components/Tutorial.tsx` to add or reorder steps.
