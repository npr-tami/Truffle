# Vendored crates

## librespot-core 0.8.0

Copied unchanged from crates.io except `src/session.rs` `check_catalogue`: librespot closes the
whole program (`exit(1)`) when the account isn't Premium. Truffle only logs it, so free accounts
can use the library with the YouTube sound source. Keep the version at 0.8.0 so every
`librespot-*` crate uses this copy. To update: copy the new release over this folder and make the
same change.
- `src/authentication.rs`: one `#[expect(deprecated)]` made `#[allow]` (the expectation isn't met with this toolchain and warned).

librespot is MIT licensed: <https://github.com/librespot-org/librespot/blob/master/LICENSE>.
