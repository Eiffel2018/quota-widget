# 0.3.18 Windows validation

- `cargo fmt --all -- --check` passed.
- `cargo test --offline --locked` passed: 190 library tests, 70 UI tests, and all integration tests; three explicit performance benchmarks remain ignored.
- `cargo build --release --locked --offline` passed on Windows x64.
- Deployed to `N:\PublicRepo\quota-widget-windows-x64\quota-widget.exe`; SHA-256: `5503404AB67396EA5AE428F334F55F580F67C7A1440C1DE108069D236A2D819A`.
- User data was not included in the source or binary package.

The quota-history UI tests cover UTC+08 midnight boundaries, first-seen percentage transitions, repeated transition suppression, unique Task counting per model family, and one-row-per-frame progressive rendering.
