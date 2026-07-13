# cpsbuild 完全実装プロンプト

## 背景と目的

CPSI（Clos Package System Installer）は CPS Linux 向けの軽量パッケージマネージャーです。現在、`cpsi` コマンド（パッケージのインストール・更新・削除）と `.clos` パッケージ形式、リポジトリインデックス（`Packages.parquet`）、Minisign署名、Apache Parquetベースのインストール済みDBは実装済みです。

しかし、**パッケージを生成するビルドツール `cpsbuild` は未実装**です。`docs/build-system.md` および `docs/package-format.md` に定義された責務に従い、`.cpsb` ビルドレシピから `.clos` 配布パッケージを生成する `cpsbuild` を新規に作成してください。

## 責務分離

| コンポーネント | 責務 |
|---|---|
| `.cpsb` | ビルドレシピ（ソース取得・ビルド手順・メタデータ） |
| `cpsbuild` | `.cpsb` を読み込み、`.clos` を生成する |
| `.clos` | 配布・インストール用パッケージ |

`cpsbuild` は **ビルド専用**であり、`cpsi` のインストール処理やリポジトリ運用とは分離されています。

## 実装場所

- 新規プロジェクト: `/home/konoha/develop/cpsbuild/`
- 言語: Rust（edition 2024）
- 既存の `cps-common` crate を共有型定義として再利用すること
  - 現状の `cps-common` は `/home/konoha/develop/cpsi/vendor/cps-common/` に存在
  - 新規 `cpsbuild` でも `vendor/cps-common` として git submodule 化するか、適切に path/git 依存で参照すること
  - `cps-common` の変更が必要な場合は、まず `cps-common` を修正し、`cpsi` との互換性を保つこと

## 参考ドキュメント

`cpsi` リポジトリ内の以下ドキュメントを必ず読み込み、実装に反映すること:

- `docs/architecture.md`：全体アーキテクチャ
- `docs/build-system.md`：ビルドシステムの責務
- `docs/package-format.md`：`.clos` 内部構造と `.pkg/info` 形式
- `docs/scripts.md`：`pre`/`post` スクリプト
- `docs/dependencies.md`：依存関係記法（`=`, `>`, `>=`, `<`, `<=`）と `provides`
- `docs/versioning.md`：バージョン・release・アーキテクチャ
- `docs/signatures.md`：Minisign署名
- `docs/repository.md`：`Packages.parquet` リポジトリインデックス

## 1. `.cpsb` ビルドレシピ形式

`.cpsb` は **ディレクトリ**として扱う。末尾に `.cpsb` 拡張子を持つ。

```text
firefox-139.0.cpsb/
├── recipe.toml          # 必須。レシピ本体
├── patches/             # 任意。パッチファイル
│   ├── 01-fix-foo.patch
│   └── 02-disable-bar.patch
├── files/               # 任意。ソース外の追加ファイル
│   └── my-config.conf
└── scripts/             # 任意。ビルド補助スクリプト
    └── generate-icon-cache.sh
```

### `recipe.toml` 仕様

```toml
[package]
name = "firefox"
version = "139.0"          # x.y または x.y.z
release = 1                # ディストリビューション修正版番号
description = "Web Browser"
license = "MPL-2.0"
arch = ["x86_64"]          # 1つ以上。x86_64/aarch64/riscv64

# 実行時依存関係（.clos の .pkg/info に書き出される）
depends = [
    "glibc>=2.42",
    "gtk4>=4.18",
]

# 提供機能（任意）
provides = [
    "browser"
]

# ビルド時のみ必要な依存（cpsbuild が警告を出す。実際の解決は将来拡張）
# build-depends = [
#     "gcc",
#     "make",
# ]

[source]
# 以下から1つを選択
# url: ソースアーカイブをHTTP/HTTPSで取得
url = "https://archive.mozilla.org/pub/firefox/releases/139.0/source/firefox-139.0.source.tar.xz"
# sha256: url とセットで必須
sha256 = "abcdef1234567890..."

# または git 取得
# git = "https://github.com/mozilla/gecko-dev"
# tag = "v139.0"
# # commit = "..."   # tag と排他利用可

# または ローカルパス（開発用）
# path = "./local-source"

[[patch]]
file = "patches/01-fix-foo.patch"
# strip = 1          # デフォルト 1

[[patch]]
file = "patches/02-disable-bar.patch"

[build]
# 各フェーズはシェルスクリプトとして実行される
# 使用可能な環境変数:
#   $PKG_NAME            package name
#   $PKG_VERSION         package version
#   $PKG_RELEASE         package release
#   $PKG_ARCH            対象アーキテクチャ（スペース区切り）
#   $PKG_RECIPE_DIR      .cpsb レシピディレクトリの絶対パス
#   $PKG_BUILD_DIR       ビルド作業ディレクトリの絶対パス
#   $PKG_SOURCE_DIR      展開済みソースディレクトリの絶対パス
#   $PKG_INSTALL_DIR     インストール先ルート（data/ の元）の絶対パス
#   $PKG_JOBS            並列ビルド数（デフォルト nproc）

prepare = """
./configure --prefix=/usr --disable-tests
"""

build = """
make -j$PKG_JOBS
"""

install = """
make DESTDIR=$PKG_INSTALL_DIR install
"""

[scripts]
# 生成する .clos の .pkg/scripts/ にコピーされる
# post: インストール後に実行される（推奨）
post = """
update-icon-caches -q /usr/share/icons/hicolor
"""

# pre: インストール前に実行される（任意）
# pre = """
# killall firefox || true
# """
```

### 必須フィールド

- `package.name`
- `package.version`
- `package.release`
- `package.arch`
- `source` セクション内の `url`/`git`/`path` のいずれか

### 任意フィールド

- `package.description`
- `package.license`
- `package.depends`
- `package.provides`
- `package.build-depends`
- `source.sha256`（URL取得時は必須）
- `source.tag` / `source.commit`
- `patch` 配列
- `build.prepare` / `build.build` / `build.install`
- `scripts.post` / `scripts.pre`

## 2. `.clos` 出力仕様

`cpsbuild build` の出力は以下のファイル:

```text
<name>-<version>-k<release>-<arch>.clos
<name>-<version>-k<release>-<arch>.clos.minisig   # --sign または設定により
```

例:

```text
firefox-139.0-k1-x86_64.clos
firefox-139.0-k1-x86_64.clos.minisig
```

### `.clos` 内部構造

`tar` アーカイブ。圧縮は **zstd** とする（`.tar.zst` としても扱えるが、拡張子は `.clos`）。

```text
/
├── .pkg/
│   ├── info              # TOML形式メタデータ
│   └── scripts/
│       ├── post          # install後スクリプト（存在する場合）
│       └── pre           # install前スクリプト（存在する場合）
└── data/
    ├── usr/
    ├── etc/
    └── ...
```

### `.pkg/info` 内容

`cps-common` の `Package` 型を元に、以下を TOML で出力:

```toml
name = "firefox"
version = "139.0.0"
release = 1
arch = ["x86_64"]
description = "Web Browser"
license = "MPL-2.0"
package_size = 45678901
installed_size = 123456789

depends = [
    "glibc>=2.42.0",
    "gtk4>=4.18.0"
]

provides = [
    "browser"
]

repository = ""   # cpsbuild では空文字列。リポジトリ登録時に cpsi 側で設定
```

- `version` は正規化して `x.y.z` 形式で出力（`139.0` → `139.0.0`）
- `depends` は正規化して `x.y.z` 形式で出力
- `package_size` は `.clos` ファイルそのもののバイト数
- `installed_size` は `data/` 以下の展開後合計バイト数

## 3. CLI コマンド仕様

### `cpsbuild build [OPTIONS] <recipe-dir>`

`.cpsb` レシピから `.clos` をビルドする。

```bash
cpsbuild build ./firefox-139.0.cpsb
cpsbuild build --output-dir ./out ./firefox-139.0.cpsb
cpsbuild build --sign --secret-key /etc/cpsbuild/keys/mykey.key ./firefox-139.0.cpsb
cpsbuild build --jobs 8 ./firefox-139.0.cpsb
```

オプション:

| オプション | 説明 |
|---|---|
| `-o, --output-dir <DIR>` | 出力先ディレクトリ（デフォルト: カレントディレクトリ） |
| `--sign` | 生成した `.clos` に Minisign 署名を付与 |
| `--secret-key <PATH>` | 署名用秘密鍵パス（`--sign` とセット） |
| `-j, --jobs <N>` | 並列ビルド数（デフォルト: 論理CPU数） |
| `--no-clean` | ビルド後も一時ディレクトリを残す |
| `--target-arch <ARCH>` | クロスコンパイル対象アーキテクチャ（将来拡張のため予約。現状は `recipe.toml` の `arch` と一致確認） |

ビルドフロー:

1. `recipe.toml` をパースし、必須フィールドを検証
2. `source` を取得:
   - `url`: HTTP/HTTPS でダウンロードし `sha256` を検証
   - `git`: 指定 `tag` または `commit` を clone/checkout
   - `path`: ローカルディレクトリをコピー
3. ソースを `$PKG_BUILD_DIR/source/` に展開
4. `patch` を順番に適用（`patch -p<strip>`）
5. `build.prepare` → `build.build` → `build.install` を順に実行
   - 失敗時は即座に終了コードを返し、一時ディレクトリを削除（`--no-clean` 時を除く）
6. `$PKG_INSTALL_DIR` から `.clos` 用の中間構造を作成:
   - `.pkg/info` を生成
   - `scripts.post` / `scripts.pre` を `.pkg/scripts/` にコピー
   - `$PKG_INSTALL_DIR` 内のファイルを `data/` に移動
7. `tar` + `zstd` でアーカイブ化し `.clos` として出力
8. `package_size` / `installed_size` を計測し `.pkg/info` を更新して再アーカイブ化
   - または、先にサイズを計測してから1回でアーカイブ
9. `--sign` 指定時は `.clos.minisig` を生成
10. 一時ディレクトリを削除（`--no-clean` 時を除く）

### `cpsbuild init [OPTIONS] <name>`

新規 `.cpsb` レシピの雛形を生成する。

```bash
cpsbuild init mypkg
cpsbuild init --version 1.0.0 --arch x86_64 mypkg
```

出力:

```text
mypkg-1.0.0.cpsb/
├── recipe.toml
├── patches/
└── files/
```

生成される `recipe.toml` は最小限のテンプレートとし、ユーザーが埋めるコメントを含める。

### `cpsbuild clean <recipe-dir>`

指定レシピに対応する一時ビルドディレクトリを削除する。

```bash
cpsbuild clean ./firefox-139.0.cpsb
```

### `cpsbuild keygen [OPTIONS] <name>`

Minisign 鍵ペアを生成する。

```bash
cpsbuild keygen myrepo
```

出力:

```text
/etc/cpsbuild/keys/myrepo.key       # 秘密鍵
/etc/cpsbuild/keys/myrepo.pub       # 公開鍵
```

### `cpsbuild sign [OPTIONS] <clos-file>`

既存の `.clos` に署名を付与する。

```bash
cpsbuild sign --secret-key /etc/cpsbuild/keys/myrepo.key firefox-139.0-k1-x86_64.clos
```

### `cpsbuild verify [OPTIONS] <clos-file>`

`.clos` の構造と `.pkg/info` の整合性を検証する。

```bash
cpsbuild verify firefox-139.0-k1-x86_64.clos
```

検証内容:

- `.clos` が有効な tar.zst アーカイブであること
- `.pkg/info` が存在し、有効な TOML であること
- 必須フィールドが存在すること
- `arch` が有効な値であること
- `depends` / `provides` の形式が有効であること

### `cpsbuild repo-index [OPTIONS] <directory>`

指定ディレクトリ内の `.clos` ファイルから `Packages.parquet` を生成する。

```bash
cpsbuild repo-index --output ./out/Packages.parquet ./out/
```

生成内容:

- 各 `.clos` の `.pkg/info` を読み込み
- `cps-common` の `RepositoryParquetFormat`（`Vec<Package>`）を作成
- Parquet 形式で出力
- `.clos` の `sha256` ハッシュも含める
  - ただし `Package` 型に sha256 フィールドがない場合は、Parquet スキーマを拡張するか、別途検討

## 4. ビルドパイプライン詳細

### 一時ディレクトリ構造

```text
/tmp/cpsbuild-<name>-<random>/
├── source/              # ソース展開先
├── build/               # ビルド作業用（configure/make 等）
├── install/             # make install 先（$PKG_INSTALL_DIR）
└── staging/             # .clos 中間構造
    ├── .pkg/
    │   ├── info
    │   └── scripts/
    └── data/
```

### 環境変数

ビルドスクリプト実行時に以下の環境変数を設定:

| 変数 | 内容 |
|---|---|
| `PKG_NAME` | パッケージ名 |
| `PKG_VERSION` | 正規化されたバージョン（`x.y.z`） |
| `PKG_RELEASE` | release 番号 |
| `PKG_ARCH` | 対象アーキテクチャ（スペース区切り） |
| `PKG_RECIPE_DIR` | `.cpsb` ディレクトリの絶対パス |
| `PKG_BUILD_DIR` | 一時ビルドディレクトリの絶対パス |
| `PKG_SOURCE_DIR` | ソース展開ディレクトリの絶対パス |
| `PKG_INSTALL_DIR` | `make install` 先の絶対パス |
| `PKG_JOBS` | 並列ビルド数 |
| `PATH` | 既存の `PATH` を継承 |
| `HOME`, `USER` | 既存の環境を継承 |

### パッチ適用

- `patch -p<strip> -i <patch-file>` を使用
- 適用失敗時はエラー終了
- パッチは `recipe.toml` の記載順に適用

### スクリプト実行

- `/bin/sh -c '<script>'` として実行
- 作業ディレクトリは `$PKG_SOURCE_DIR`
- 標準出力・標準エラーはそのまま表示
- 終了コード 0 以外はエラー（`install` フェーズも同様）

## 5. 署名と鍵管理

### Minisign

- 署名方式: Minisign
- 暗号方式: Ed25519
- 署名対象: `*.clos`

### 鍵保存場所

```text
/etc/cpsbuild/keys/
```

### API / コマンド

- `cpsbuild keygen <name>`: `name.key` / `name.pub` を生成
- `cpsbuild build --sign --secret-key <path>`: ビルド時署名
- `cpsbuild sign --secret-key <path> <clos>`: 事後署名

秘密鍵はパスフレーズで保護される場合があるため、パスフレーズ入力を対話的に求めるか、環境変数 `CPSBUILD_SECRET_KEY_PASSWORD` で受け取る。

## 6. プロジェクト構造

```text
cpsbuild/
├── Cargo.toml
├── README.md
├── src/
│   ├── main.rs              # CLIエントリーポイント
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── build.rs         # build コマンド
│   │   ├── init.rs          # init コマンド
│   │   ├── clean.rs         # clean コマンド
│   │   ├── keygen.rs        # keygen コマンド
│   │   ├── sign.rs          # sign コマンド
│   │   ├── verify.rs        # verify コマンド
│   │   └── repo_index.rs    # repo-index コマンド
│   ├── recipe/
│   │   ├── mod.rs
│   │   └── parser.rs        # recipe.toml パース
│   ├── source/
│   │   ├── mod.rs
│   │   ├── fetch.rs         # URLダウンロード
│   │   ├── git.rs           # git clone
│   │   └── local.rs         # ローカルパス
│   ├── build/
│   │   ├── mod.rs
│   │   └── runner.rs        # ビルドパイプライン実行
│   ├── package/
│   │   ├── mod.rs
│   │   └── archive.rs       # .clos 生成
│   ├── signature/
│   │   ├── mod.rs
│   │   └── minisign.rs      # Minisign署名
│   ├── repository/
│   │   ├── mod.rs
│   │   └── parquet.rs       # Packages.parquet 生成
│   └── util/
│       ├── mod.rs
│       ├── constants.rs     # パス等の定数
│       └── net.rs           # ダウンロード（cpsi と似た実装）
└── tests/                   # 統合テスト
    └── fixtures/
        └── minimal.cpsb/
```

## 7. 依存クレート

`Cargo.toml` に以下を追加（必要に応じて）:

```toml
[dependencies]
clap = { version = "4.6", features = ["derive"] }
tokio = { version = "1.48", features = ["fs", "io-util", "macros", "rt-multi-thread"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream", "blocking"] }
serde = { version = "1.0", features = ["derive"] }
toml = "0.8"
tar = "0.4"
zstd = "0.13"
sha2 = "0.10"
hex = "0.4"
minisign = "0.7"           # Minisign署名生成用
minisign-verify = "0.2"    # 検証用
cps-common = { path = "vendor/cps-common" }
thiserror = "2.0"
parquet = "58.3"
arrow = "58.3"
serde_arrow = { version = "0.14", features = ["arrow-58"] }
tempfile = "3.27"
indicatif = "0.17"
```

## 8. エラー処理

`cps-common` の `CpsiError` をそのまま使用するか、`cpsbuild` 用に `CpsbuildError` を新たに定義する。

`CpsbuildError` を新設する場合、以下の変種を含める:

- `RecipeParseError(String)`
- `InvalidRecipe(String)`
- `SourceFetchError(String)`
- `ChecksumMismatch { expected: String, actual: String }`
- `PatchApplyError { patch: PathBuf, output: String }`
- `BuildScriptFailed { phase: String, exit_code: i32 }`
- `ArchiveError(String)`
- `SigningError(String)`
- `InvalidPackage(String)`

## 9. テスト要件

### ユニットテスト

- `recipe.toml` パース（必須フィールド欠落、無効な arch、無効な依存関係）
- 依存関係文字列の正規化（`2.42` → `2.42.0`）
- `.clos` ファイル名生成
- パッチストリップレベル適用

### 統合テスト

以下は可能な限り `/tmp/opencode/` 以下の一時ディレクトリで実施:

1. **最小パッケージビルド**: ローカルソースから `.clos` を生成し、内部構造を検証
2. **URLソース取得**: HTTP mock または小さなファイルでダウンロード・sha256検証
3. **パッチ適用**: パッチ付きレシピでソース変更が反映されることを確認
4. **pre/post スクリプト**: スクリプトが `.clos` に含まれることを確認
5. **署名**: 鍵生成 → 署名 → 検証 の一連の流れ
6. **repo-index**: 複数 `.clos` から `Packages.parquet` を生成し、`cpsi` のリポジトリ読み込みで使用できることを確認
7. **verify コマンド**: 不正な `.clos` を検出

### CI / 検証

- `cargo check` が警告なしで通ること
- `cargo test` が全て成功すること
- `cargo clippy` を実行し、警告を解消すること
- `cargo fmt` で整形済みであること

## 10. 実装チェックリスト

- [ ] 新規プロジェクト `/home/konoha/develop/cpsbuild/` を作成
- [ ] `cps-common` を共有型として組み込み
- [ ] `cpsbuild init` コマンド
- [ ] `cpsbuild build` コマンド
  - [ ] `recipe.toml` パース
  - [ ] URLソース取得 + sha256検証
  - [ ] gitソース取得
  - [ ] ローカルソース取得
  - [ ] パッチ適用
  - [ ] ビルドフェーズ実行（prepare/build/install）
  - [ ] `.clos` アーカイブ生成
  - [ ] `.pkg/info` 生成（サイズ計算含む）
- [ ] `cpsbuild clean` コマンド
- [ ] `cpsbuild keygen` コマンド
- [ ] `cpsbuild sign` コマンド
- [ ] `cpsbuild verify` コマンド
- [ ] `cpsbuild repo-index` コマンド
- [ ] 包括的なテスト
- [ ] `README.md`（ビルド手順・使用例）

## 11. 制約と注意事項

- **ビルドと配布を分離**: `cpsbuild` は `.clos` 生成のみを行い、インストール処理は絶対に実装しないこと。
- **自己完結パッケージ**: 生成する `.clos` は `data/` 内にすべてのファイルを含み、ビルド環境に依存しないこと。
- **シンプルさ**: 過度な抽象化を避け、人間が読みやすいコードを維持すること。
- **既存 `cpsi` との互換性**: 生成した `.clos` が `cpsi install` で正しくインストールできるよう、`.pkg/info` 形式と `scripts` の配置を厳密に守ること。
- **プレリリース**: バージョンは `x.y` または `x.y.z` のみ対応。`-alpha`/`-beta` 等は非対応（`cpsi` 仕様と一致）。
- **アーキテクチャ**: 現時点では `x86_64`, `aarch64`, `riscv64` のみ。

## 12. 成果物

最終的に以下を `/home/konoha/develop/cpsbuild/` 以下に作成すること:

- コンパイル可能な Rust プロジェクト
- 上記すべてのコマンド
- 包括的なテストスイート
- `README.md`
- `cpsi` で検証可能なサンプル `.cpsb`（`tests/fixtures/minimal.cpsb/` 等）
