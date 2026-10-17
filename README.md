# rtail

[![CI](https://github.com/andersonmrodrigues/tail-web/actions/workflows/ci.yml/badge.svg)](https://github.com/andersonmrodrigues/tail-web/actions/workflows/ci.yml)

`tail` rewritten in Rust: it seeks backward through huge files, follows them with
native OS notifications (kqueue on macOS, inotify on Linux), survives truncation
and log rotation, and can stream the files it follows to a browser.

> Work in progress. Static tail (`-n`, `-c`, `+N`, several files, stdin) is in place.

## Build

```bash
cargo build --release
./target/release/rtail -n 20 /var/log/system.log
```

## License

[MIT](LICENSE)
