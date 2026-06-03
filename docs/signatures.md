# 署名システム

CPSI はパッケージ署名に Minisign を採用予定です。

## 方式

署名方式:

```text
Minisign
```

暗号方式:

```text
Ed25519
```

## 署名対象

```text
*.clos
Packages.parquet
```

理由:

- リポジトリメタデータ改ざん防止

## リポジトリ鍵

想定運用:

```text
公開鍵
↓
初回登録時取得
↓
利用者が信頼確認
↓
ローカル保存
```

確認例:

```text
Fingerprint:
SHA256:XXXXXXXXXXXX

Trust this key? [y/N]
```

## 保存先候補

```text
/etc/cpsi/keys/
```
