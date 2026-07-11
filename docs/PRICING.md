# Pricing Model and Catalog Maintenance

Token Ledger calculates reproducible API list-price equivalents. It does not calculate a provider invoice unless the user separately supplies complete, bounded billing attestations.

## Separate measures

The reporting model keeps these values independent:

- provider-reported token counters;
- API-equivalent USD list-price estimates;
- provider units such as eligible Codex credits;
- user-recorded charges and refunds;
- attested actual billed amounts; and
- imported provider reconciliation buckets.

Subscription inclusion, prepaid balances, taxes, discounts, negotiated rates, fixed fees, and account allocation are not inferred.

## Catalog files

- `assets/prices.json` is the bundled effective-dated catalog.
- `assets/prices.manifest.example.json` demonstrates a manifest that binds a catalog revision and SHA-256 digest.

Each rate records its provider, canonical model, measure, unit, token-class rates, effective boundary, and evidence. Model aliases are resolved by effective date so a historical report does not silently inherit a future alias or price.

## Status semantics

- **Exact**: every required observed component has one matching rule.
- **Range**: documented alternatives or unresolved bounded dimensions produce finite lower and upper estimates.
- **At least / partial**: known components have a subtotal but one or more components are unpriced.
- **Unpriced**: no defensible numeric subtotal exists.
- **Unavailable**: the catalog explicitly says the measure does not apply under the observed conditions.

Unknown never means free. An explicitly free component may be zero, but it cannot turn other missing components into zero.

## Evidence requirements

Catalog changes must use primary provider documentation whenever available. Evidence records should include the exact URL, retrieval timestamp, and a short description of the supported fact. Avoid copying provider prose beyond what is needed to identify the rule.

The bundled catalog currently cites official OpenAI and Anthropic documentation. Provider and model names are used only to identify compatibility and pricing rules; Token Ledger is not affiliated with or endorsed by either provider.

## Updating the catalog

1. Confirm the model identity and documented rate on a primary source.
2. Record the published effective date. If none is published, do not backcast the rule before the verified observation boundary.
3. Encode rates as decimal strings with an explicit denominator.
4. Add or update aliases without erasing historical mappings.
5. Increment the immutable catalog revision.
6. Update publication, verification, and staleness timestamps.
7. Recompute the manifest SHA-256 over the exact catalog bytes.
8. Run catalog validation, boundary, alias, dimension, and CLI lifecycle tests.
9. Review `ledger prices diff`, `ledger prices verify`, and a representative historical cost report.

Catalog installation is atomic. Replaced and installed revisions are retained for reproducible reports and explicit rollback.

## User overrides

A pricing-dimension override must be bounded by provider, model, dimension, value, effective interval, attestation time, and note. It resolves only a documented catalog alternative. It cannot invent a price or apply an unbounded present-day fact to historical usage.

## Correctness reports

A useful price issue identifies the catalog revision, model, report date, observed dimensions, expected status, primary source URL, and the exact arithmetic. Do not attach session logs or account screenshots containing private information.
