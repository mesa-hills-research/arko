# The remote web page

Arko's remote web page, written with [Leptos](https://leptos.dev) and built for the browser as
WebAssembly. Arko serves it at the root of `/remote`'s address.

## Build

Once, install Trunk:

```sh
cargo install --locked trunk
```

Then build:

```sh
web/build.sh
```

`rust-toolchain.toml` here pins the compiler and adds the WebAssembly target, which rustup installs
on the first build, and Trunk downloads the matching `wasm-bindgen` and `wasm-opt`. The output goes
to `src/remote/page/`, which arko-tui embeds, so building arko-tui needs none of these tools. Commit
the output with the change that made it. The build leaves this computer's paths out of the output,
so CI can rebuild it and run `git diff --exit-code src/remote/page web/style`.

Before each build, `theme-css` writes `style/themes.css` and `style/all-themes.css` from the theme
files in `../themes/`, the ones the apps use. The icons and the font in `assets/` are copied into
the output.

## Test

```sh
cargo test --manifest-path web/Cargo.toml
```

## Try it

Arko's scripted demo runs without a model. From the repository's root:

```sh
ARKO_DEMO_ITEMS=2000 cargo test --lib remote_page_demo -- --ignored --nocapture
```

Open the link it prints, then send "Run the demo" or "demo:stream". `src/remote_demo.rs` lists the
other scripts and settings.
