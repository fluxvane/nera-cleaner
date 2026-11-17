# Nera Cleaner — Design rules

The old app looked messy because every screen invented its own layout, spacing and borders. These rules make every screen look like one product. They are enforced by building screens only from `crates/nera-ui`.

## Shell

- Sidebar (232 px) on the left, grouped: Overview · Clean · Storage · System · Protect; Settings pinned in the footer.
- The content area is one `Page`:
  - **Header** (64 px, hairline bottom border): title + one-line subtitle on the leading edge, actions on the trailing edge. The primary action is last and the only `.primary()` button on the page.
  - **Toolbar** (optional band): search, filters, segmented views, selection summary.
  - **Body**: scrolls; content max 1080 px wide, centred, 32 px side gutters, 24 px top/bottom; sections 24 px apart.
  - **Footer** (optional sticky bar): selection summary + the commit action ("Clean 1.2 GB").

## Spacing (`ui::sp`)

| Token | px | Use |
|---|---|---|
| XXS | 2 | title ↔ subtitle inside a row |
| XS | 4 | badge groups, icon ↔ label in tight controls |
| SM | 8 | buttons in a group, icon ↔ label |
| MD | 12 | section title ↔ content, row column gap |
| LG | 16 | card padding, grid gaps, row horizontal padding |
| XL | 24 | between sections, page vertical padding |
| XXL | 32 | page side gutters, empty-state breathing room |

Equal gaps mean equal relationships. Never add padding around a `Card` inside a `Section` (no double padding).

## Surfaces

- One card style (`Card`, `card_surface`): theme background, 1 px `theme.border`, `radius_lg`, no shadow.
- Lists: `ListCard` with `Row`s separated by hairlines; optional header row (`list_header`) on `theme.secondary`.
- Rows keep fixed lanes: checkbox · icon tile (32 px) · title/subtitle (flex) · badges · value (min 88 px, right-aligned) · actions.
- Metrics: `StatGrid` of `StatTile`s (2–4 columns).
- States: `EmptyState` (nothing yet / all clean / error), `ProgressPanel` (running), `Banner` (permission missing, warnings).

## Colour

- Only `cx.theme()` tokens and `ui::Tone` (Neutral, Accent, Success, Warning, Danger, Info). Light and dark both work automatically.
- Tone is for meaning (usage level, status, destructive), not decoration. Danger is reserved for destructive actions and critical states.

## Typography

- Page title `text_lg` semibold; section title `text_base` semibold; body `text_sm`; captions `text_xs` muted.
- Big numbers only in `StatTile` (`text_2xl`).

## Interaction

- Every destructive action shows a confirmation (`ui::confirm`) naming what will be removed and how much; say whether it goes to the Trash.
- Long work shows a `ProgressPanel` with Cancel; results replace it.
- After an action, show the outcome (toast via `ui::notify` and updated content), including partial failures.
- Disabled buttons while work is running; never let the user start the same job twice.
- Icons: Lucide via `ui::icon("name")`. One icon family.
