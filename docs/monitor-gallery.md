# Terminal monitor screenshot gallery

The **Overview** image is the user's saved screenshot from a signed-in monitor session. It shows reported request/token totals and estimated costs as they appeared at capture time; it does not show prompts or credentials. The other five panes are synthetic `TestBackend` renders and are not screenshots of a signed-in account.

The preview in the [repository README](../README.md) links here. The five synthetic panes were rendered at 150 × 38 terminal cells; Catalog uses 150 × 44 so the long-context prices are visible. Their status lines mark the data as synthetic.

In the synthetic samples, Models and Sessions summarize 24 events. The Requests image shows only three illustrative recent rows to keep the view legible; those rows are not a complete reconstruction of the sample aggregates or the live Overview values.

## Overview

![Saved live Overview pane showing reported request/token totals, estimated costs, and rolling-window table](../assets/screenshots/overview.png)

## Quota

![Quota pane with synthetic premium-request allowance, remaining amount, and reset information](../assets/screenshots/quota.png)

## Models

![Models pane showing synthetic per-model token counts, costs, and timing](../assets/screenshots/models.png)

## Requests

![Requests pane showing synthetic recent requests and token breakdowns](../assets/screenshots/requests.png)

## Sessions

![Sessions pane showing synthetic per-session request, token, cost, and rolling-rate totals](../assets/screenshots/sessions.png)

## Catalog

![Catalog pane showing synthetic model entries, reference-price details, and sort controls](../assets/screenshots/catalog.png)
