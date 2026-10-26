# The remote page in Rust

A prototype of Arko's remote web page, written with [Leptos](https://leptos.dev) and built for the
browser as WebAssembly. Arko serves it under `/next/`, beside the current page, and at `/` too when
`ARKO_REMOTE_PAGE=next` is set.

It shows the main transcript as it streams, with replies in Markdown and a line per tool call, and
it has the message box, "Latest", the light and dark themes and the theme menu. Everything else
stays on the current page.

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
to `src/remote/next/`, which arko-tui embeds, so building arko-tui needs none of these tools. Commit
the output with the change that made it. The build leaves this computer's paths out of the output,
so CI can rebuild it and run `git diff --exit-code src/remote/next web/style/themes.css`.

Before each build, `theme-css` writes `style/themes.css` from `../themes/macos-classic.json`, the
theme file the apps use.

## Try it

```sh
cargo run --example remote_demo        # from the repository's root
```

The demo serves a made-up session of 2,000 items, streams a reply every few seconds, and prints the
links to both pages.
