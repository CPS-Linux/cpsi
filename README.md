# cpsi
A package manager designed to be understood. Self-contained packages, Parquet repositories, and simple dependency resolution for CPS Linux.

## 静的バイナリのビルド

Docker BuildKit を使い、musl に静的リンクした `cpsi` を `dist/` に出力できます。

```bash
docker buildx build --target artifact --output type=local,dest=dist .
file dist/cpsi
./dist/cpsi --help
```

`vendor/cps-common` は Git submodule です。新しく clone した作業ツリーでは、先に
次のコマンドで取得してください。

```bash
git submodule update --init --recursive
```
