# Tunelith

[English](README.md) | 日本語

日本のデジタル放送 (ISDB-T, ISDB-S, ISDB-S3 (4K/8K)) のチューナを扱う，低レベルで
クロスプラットフォームな抽象化層です．Rust で書かれています．

Tunelith は **放送方式・周波数・ストリーム ID** (ISDB-S は TSID，ISDB-S3 は TLV
ストリーム ID) で選局し，チャンネルリストを持ちません．チャンネル定義，スキャン，
EPG，デスクランブルは上位のレイヤに任せます．

> [!WARNING]
> Tunelith は開発の初期段階にあります．API やコマンドラインは今後変わります．

## 特徴

- **クロスプラットフォーム**: Linux，Windows，macOS，そして WebUSB 経由で
  Chromium 系ブラウザでも動作します．
- **低レベル**: 受信するものをプログラムが放送方式・周波数・ストリーム ID で
  指定し，Tunelith はそのとおりに選局します．間にチャンネルリストを挟みません．
- **ISDB-S3**: 4K/8K 放送を TLV で受信できます．
- **ユーザ空間で動作**: USB チューナはユーザ空間から USB 越しに制御するため，
  カーネルモジュールのビルドが要りません．
- **チューナの共有**: tunelithd は同じものを受信するプログラム間でチューナを
  共有し，読み出しの遅いプログラムは他を待たせず，自分のデータを取りこぼします．
- **BonDriver 互換**: [BonDriver_Tunelith](#bondriver_tunelith) により，TVTest や
  EDCB などから tunelithd 経由で受信できます (Windows / Linux)．

## 対応デバイス

| デバイス | Linux | Windows | macOS | ブラウザ (WebUSB) |
|---|:-:|:-:|:-:|:-:|
| PLEX PX-W3U4 / PX-W3PE4 / PX-W3PE5 | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| PLEX PX-Q3U4 / PX-Q3PE4 / PX-Q3PE5 | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| PLEX PX-MLT5U / PX-MLT5PE / PX-MLT8PE | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| PLEX PX-M1UR | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| PLEX PX-S1UR | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| Digibest ISDB6014 V2.0 (4TS) / e-better DTV02A-4TS-P | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| Digibest ISDB2056 / ISDB2056N / e-better DTV02A-1T1S-U | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| Digibest ISDBT2071 / e-better DTV03A-1TU | ✅[^untested] | ✅[^os] | ✅[^os] | ✅[^untested] |
| e-better DTV02A-5TS-P | ✅ | ✅[^os] | ✅[^os] | ✅ |
| PT4K (TBS6812)[^left] | ✅ | ✅[^bda] | — | — |
| Linux DVB ドライバのあるその他の ISDB チューナ | ✅[^generic] | — | — | — |

✅ 対応，🚧 対応予定，— 予定なし．

> [!TIP]
> **実機で試してくれる方を募集しています．** 上のデバイスの多くは，まだ実機で
> 試していません．お持ちの方は `tunelith list` と `tunelith tune` を試し，動いても
> 動かなくても，機種と OS を添えて [Issue](https://github.com/siketyan/tunelith/issues)
> で結果を教えてください．

[^left]: ISDB-S3 は右旋の 4K 放送でのみ確認しています．左旋の放送 (NHK BS8K) は，
    手元のアンテナではどのツールでも受信できませんでした．
[^untested]: DTV02A-5TS-P とともに px4_drv から移植しましたが，実機ではまだ
    試していません．報告を歓迎します．
[^generic]: カーネルドライバが対応するものをそのまま扱います．機種固有の処理は
    専用のドライバで行います．
[^bda]: TBS の BDA ドライバを通して扱います．Windows からはまだカードで LNB に給電できず，
    `--lnb` は失敗します．LNB には別の手段で給電してください．
[^os]: CI でその OS 向けにビルドしテストが通っていますが，まだ実機では動かして
    いません．Windows ではデバイスに WinUSB を割り当てる必要があります．

USB ドライバは [nusb](https://github.com/kevinmehall/nusb) の上でユーザ空間で動作し，
Linux，Windows (WinUSB)，macOS，Chromium 系ブラウザ (`tunelith-wasm` による WebUSB)
で動きます．Windows の PT4K は BDA ドライバを通して扱い，Tunelith はそれを DirectShow の
グラフを使わず Kernel Streaming で直接操作します．

PX-Q 系は 1 枚のカードに 2 枚のボードを載せたものです．Tunelith はこれを 8 チューナの
1 デバイスにまとめ，電源もまとめて制御します．

## クレート

| クレート | 内容 | ライセンス |
|---|---|---|
| `tunelith-core` | 公開する型，`Driver` / `Device` / `Tuner` トレイト，`Registry`，汎用の Linux DVB ドライバと Windows BDA ドライバ，nusb による USB トランスポート，tunelithd のプロトコル | MIT OR Apache-2.0 |
| [`tunelith`](crates/tunelith) | チューナを使うプログラムのための tunelithd クライアント | MIT OR Apache-2.0 |
| `tunelith-driver-pt4k` | 汎用の DVB・BDA ドライバの上に構築した PT4K (TBS6812) ドライバ | MIT OR Apache-2.0 |
| `tunelith-driver-px4` | px4_drv から移植した PLEX / e-better / Digibest の USB チューナのドライバ | GPL-2.0-only |
| `tunelith-bondriver` | tunelithd 経由で受信する BonDriver，BonDriver_Tunelith | MIT OR Apache-2.0 |
| `tunelith-cli` | `tunelith` コマンドと，プログラム間でチューナを共有するデーモン tunelithd | GPL-2.0-only |

`tunelith-driver-px4` は [px4_drv](https://github.com/tsukumijima/px4_drv) の移植で，
そのライセンスである GPL-2.0-only に従います．詳しくは [PROVENANCE.md](crates/tunelith-driver-px4/PROVENANCE.md)
を参照してください．これをリンクするプログラムも GPL に従います．

## はじめに

### ビルド

ツールチェーンは `rust-toolchain.toml` で固定しています．

```shell
cargo build --release
```

### ファームウェア

USB チューナには IT930x のファームウェア `it930x-firmware.bin` が必要ですが，
Tunelith には同梱していません．次のいずれかに置いてください．

- `/lib/firmware/`
- `$XDG_DATA_HOME/tunelith/firmware/` (既定では `~/.local/share/tunelith/firmware/`)
- Windows では `%ProgramData%\tunelith\firmware\`

px4_drv をインストール済みなら，ファイルはすでに `/lib/firmware/` にあります．

### 権限 (Linux)

USB チューナは usbfs を通して制御するため，デバイスノードへの書き込み権限が必要です．
[`packaging/udev/70-tunelith.rules`](packaging/udev/70-tunelith.rules) は，`video`
グループと，シートにログインしているユーザにこれを与えます．

```shell
sudo install -m644 packaging/udev/70-tunelith.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

px4_drv のカーネルモジュールが読み込まれている場合，Tunelith はデバイスを開くときに
モジュールから引き取ります．Tunelith だけがデバイスを使うよう，モジュールは
ブラックリストに入れてください．DVB チューナには `/dev/dvb` へのアクセスが必要で，
通常は `video` グループで与えられます．

## 使い方

### tunelithd

tunelithd はデバイスを保持し，プログラム間でチューナを共有します．プログラムが
受信したいもののストリームを要求すると，空いているチューナか，別のプログラムのために
すでに同じものを受信しているチューナのストリームが渡されます．

```shell
tunelithd
```

`/run/tunelith/tunelithd.sock` (Windows では名前付きパイプ `\\.\pipe\tunelith`)，
または `--socket` や `TUNELITH_SOCKET` で指定した場所で待ち受け，`video` グループ
(または `--socket-group` で指定したグループ) のメンバーにチューナの利用を許可します．

Linux では，systemd でシステム全体用か，ユーザ用として動かせます．ユニットは
[`packaging/systemd`](packaging/systemd) とリリースアーカイブに含まれています．

- **システム**: 専用のユーザで動作し，`video` グループを通してチューナにアクセスし，
  共有します．

  ```shell
  sudo install -m644 packaging/systemd/system/tunelithd.service /etc/systemd/system/
  sudo systemctl enable --now tunelithd
  ```

- **ユーザ**: そのユーザとして動作し，そのユーザだけが
  `$XDG_RUNTIME_DIR/tunelith/tunelithd.sock` で利用できます．`tunelith` コマンドは
  システムのソケットより先にこのソケットを探します．

  ```shell
  install -Dm644 packaging/systemd/user/tunelithd.service ~/.config/systemd/user/tunelithd.service
  systemctl --user enable --now tunelithd
  ```

どちらも `tunelithd` を `/usr/local/bin` や `/usr/bin` などから探します．
`~/.cargo/bin` など別の場所にある場合は，`systemctl edit tunelithd` (ユーザ用は
`--user` を付けて) で指定してください．USB チューナにはどちらの場合も上記の udev
ルールが必要です．

### `tunelith` コマンド

コマンドは tunelithd を経由するか，`--direct` を付けると自身でデバイスを開きます．

デバイスとチューナを一覧表示します．

```shell
tunelith list
```

選局し，ストリームを標準出力に書き出します．

```shell
# ISDB-T: 周波数 (kHz)．
tunelith tune --system isdb-t --freq 521143 > out.ts

# ISDB-S: LNB で変換する前のダウンリンク周波数 (kHz) と TSID．
tunelith tune --system isdb-s --freq 11727480 --stream-id 0x4010 > out.ts

# ISDB-S3: TLV ストリーム ID．左旋の放送には `--polarization left` を付けます．
tunelith tune --system isdb-s3 --freq 12034360 --stream-id 0xB110 > out.tlv
```

| オプション | 説明 |
|---|---|
| `--system` | `isdb-t`，`isdb-s`，`isdb-s3` のいずれか |
| `--freq` | 放送の周波数 (kHz)．衛星ではダウンリンク周波数 (BS-1 なら 11727480) で，Tunelith が LNB 向けに変換します |
| `--stream-id` | ISDB-S では TSID，ISDB-S3 では TLV ストリーム ID．10 進数，または `0x` を付けた 16 進数．相対 TS 番号は受け付けません |
| `--polarization` | `right` (既定) または `left` |
| `--tuner` | 使うチューナ (`list` の表示どおり)．省略するとその放送方式を受信できる最初の空きチューナ |
| `--lnb` | アンテナの LNB に給電します |
| `--duration` | 指定した秒数で停止します |
| `--direct` | tunelithd を経由せず，デバイスを直接開きます |
| `--socket` | tunelithd のソケット (または `TUNELITH_SOCKET`)．省略するとユーザの tunelithd があればそれ，なければシステムのもの |

ストリームは ISDB-T と ISDB-S では MPEG-2 TS，ISDB-S3 では TLV です．

### チューナを使うプログラム

プログラムは [`tunelith`](crates/tunelith) クレートで tunelithd 経由で受信できます．
その README と [API ドキュメント](https://siketyan.github.io/tunelith/tunelith/)
を参照してください．

### BonDriver_Tunelith

BonDriver_Tunelith は，TVTest や EDCB などのための BonDriver (IBonDriver2) で，
tunelithd 経由で受信します．`cargo build --release -p tunelith-bondriver` でビルドすると，
Windows (x64) では `BonDriver_Tunelith.dll`，Linux では `libBonDriver_Tunelith.so`
ができます．

Tunelith はチューニング空間とチャンネルのリストを持たないため，これらはライブラリと
同じ名前で横に置いた TOML ファイル (`BonDriver_Tunelith.dll` の横の
`BonDriver_Tunelith.toml`) に書きます．

```toml
# tunelithd のソケット．省略するとユーザまたはシステムのもの．
# socket = '\\.\pipe\tunelith'
# アンテナの LNB に給電します．
lnb = false

[[space]]
name = "UHF"
system = "isdb-t"
channel = [
  { name = "13ch", frequency = 473143 },
  { name = "14ch", frequency = 479143 },
]

[[space]]
name = "BS"
system = "isdb-s"
channel = [
  { name = "BS01/TS0", frequency = 11727480, stream_id = 0x4010 },
]
```

チャンネルには `tunelith tune` と同じもの，つまり kHz 単位の `frequency`，衛星では
`stream_id`，そして `polarization` を指定します．ライブラリを別の名前でコピーし，
それぞれにファイルを置けば，別のリストを持たせられます．信号レベルには C/N を返します．
Linux では BonDriverProxy_Linux などのホストに読み込ませるもので，ホストの
`dynamic_cast` のために C++ ランタイムをリンクします．

## 対象外

- チャンネルリスト，チャンネルスキャン，サービス分離，EPG．
- デスクランブル (B-CAS / ACAS)．
- BonDriver DLL の読み込み，recpt1 互換のコマンドライン，Mirakurun 互換の API．
- 旧 PLEX 機種の TS 難読化．

## ライセンス

各クレートは上の表のライセンスに従います．IT930x のファームウェアは Tunelith に
含まれません．
