# `--align-raw`

Controls how RAW+JPEG pairs are treated when both an Original and Alternative version exist for an asset.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --align-raw original
```

## Details

- **Default**: `as-is`
- **Type**: Enum
- **Values**: `as-is`, `original`, `alternative`

| Policy | Behavior |
|--------|----------|
| `as-is` | No change — download versions exactly as iCloud reports them |
| `original` | If the Alternative version is a RAW file, swap it into the Original slot (RAW becomes the primary download) |
| `alternative` | If the Original version is a RAW file, swap it into the Alternative slot (JPEG becomes the primary download) |

The swap only occurs when the target version's `asset_type` contains `"raw"`. If no Alternative version exists, or the type doesn't match, no swap is performed regardless of policy.

## Example

A camera that shoots RAW+JPEG may store the JPEG as Original and the RAW as Alternative. To download the RAW file as the primary:

```sh
icloudpd-rs -u my@email.address -d /photos --align-raw original
```

To ensure the JPEG is always the primary (even if iCloud puts the RAW as Original):

```sh
icloudpd-rs -u my@email.address -d /photos --align-raw alternative
```

## See Also

- [`--size`](size.md)
- [Content Filtering](../features/content-filtering.md)
