# Dashboard design system

The dashboard has one design system: a set of named tokens for color, type, spacing, radii, and shadows, and a small set of components built from them. This page explains where the tokens live and the rules for using them when you change the dashboard UI.

## Where the tokens live

The `:root` block at the top of the `<style>` element in `cloudflare/src/ui.rs` declares every token as a CSS custom property. Component rules and inline styles read them with `var(--name)`. Don't write a literal color, font size, or radius anywhere else in the dashboard markup or script.

The sign-in page that `render_login_gate_html` serves repeats the subset of tokens it needs. Those copies keep the same names and values.

## Color

The dashboard uses one dark theme.

| Role | Tokens | Use |
| --- | --- | --- |
| Surfaces | `bg-base`, `bg-panel`, `bg-card`, `bg-card-hover` | Page, panels and bars, nested cards, hover. Each step is one shade lighter. |
| Wells | `bg-input`, `bg-sunken` | Text fields, code blocks, and chips. |
| Edges | `border`, `border-control`, `border-hi` | Hairlines, outlines of anything you type into, and the focus ring. |
| Text | `text-strong`, `text-main`, `text-muted`, `text-dim` | Titles, body and values, labels, and hints. |
| Interaction | `accent`, `accent-hover`, `accent-text`, `accent-soft`, `accent-line`, `on-accent` | Primary buttons, links, the active tab, and selected segments. |
| Status | `success`, `danger`, `warning`, `info`, `violet`, `teal` | Running, deleted or OOM, pending, live controller numbers, Rust builders, and ARM64. |

Each status color comes as a set: the text color, a `-soft` fill, and a `-line` outline. A badge uses all three from one set. Buttons use the `-fill` tokens, such as `success-fill`, with `on-accent` text. Always pair a status color with a word, so the state doesn't depend on color alone.

Text tokens meet a 4.5:1 contrast ratio on the surfaces they're used on. `border` is decorative; outline inputs and selects with `border-control` so they stay visible.

## Type, spacing, and shape

- Use `font-sans` for text and `font-mono` for numbers, instance names, machine types, and commands.
- Font sizes come from `fs-xs` (11px), `fs-sm` (12px), `fs-md` (13px, the body size), `fs-lg` (15px), `fs-xl` (16px), and `fs-display` (21px).
- Spacing steps are 4px apart, from `space-1` (4px) to `space-6` (24px), plus `space-0` (2px) for badge padding.
- Radii are `radius-xs` for small pills, `radius-sm` for small buttons, `radius-md` for buttons, inputs, and cards, `radius-lg` for panels, and `radius-pill` for badges.
- Only floating elements cast shadows: `shadow-bar` on the top bar and `shadow-pop` on toasts, menus, and the status banner.

## Components

| Component | Classes |
| --- | --- |
| Button | `.btn`, with `.btn-secondary`, `.btn-emerald`, `.btn-danger`, `.btn-purple`, and `.btn-sm` |
| Badge | `.badge`, with `.badge-running`, `.badge-deleted`, `.badge-warn`, `.badge-runner`, `.badge-builder`, `.badge-arm64`, and `.badge-x86` |
| KPI card | `.kpi-grid`, `.kpi-card`, `.kpi-label`, `.kpi-value`, and `.kpi-sub` |
| Panel | `.panel`, `.panel-header`, `.panel-title`, and `.panel-subtitle` |
| Navigation tab | `.nav-tabs`, `.tab-btn`, and `.tab-badge` |
| Segmented control | `.arch-pill-group` and `.arch-pill-btn` |
| Field | `.form-grid`, `.field`, `.toggle-label`, and `.budget-input-group` |
| Table | `.table-wrap` around a `<table>` |
| Code block | `.ssh-block` |
| Stage card | `.stage-list`, `.stage-card`, and `.stage-chip` |
| Toast | `.toast` inside `#toast-container` |
| Status banner | `.status-banner` |

Charts draw axis text with `.chart-text` and gridlines with `.chart-grid`. SVG presentation attributes such as `fill="…"` can't read custom properties, so set chart colors through `style` or a class.

## Change a token

1. Edit the value in the `:root` block in `cloudflare/src/ui.rs`.
1. If the sign-in page declares the same token, update it there too.
1. Check that text on the changed surface still meets 4.5:1 contrast.
1. Run `cargo test --manifest-path cloudflare/Cargo.toml`, and keep the embedded dashboard HTML under 200 KiB.
