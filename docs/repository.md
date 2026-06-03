# リポジトリ仕様

CPSI はリポジトリインデックスとして `Packages.parquet` を使用します。

## 更新

```bash
cpsi update
```

## 更新時の動作

```text
download: Packages.parquet, Packages.parquet.minisign
↓
Packages.parquet
↓
ローカルキャッシュ更新
```

## `Packages.parquet`

`Packages.parquet` はリポジトリインデックスです。

用途:

- パッケージ検索
- 依存解決
- 情報表示
- 更新確認

## 格納例

| name | version | release | arch | sha256 |
| --- | --- | --- | --- | --- |
| firefox | 139.0 | 1 | x86_64 | ... |

## Parquet 採用理由

- 高速検索
- 列指向データ構造
- 将来的な拡張性
