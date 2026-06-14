a fast, light os for the xteink x4 e-reader.

add and remove books from your browser.

![](docs/home.png)

```sh
cargo run -p fw --release                                       # build, flash, serial monitor
cargo test -p app-core -p proto --target aarch64-apple-darwin   # host tests
```

internals: [ARCHITECTURE.md](docs/ARCHITECTURE.md)
