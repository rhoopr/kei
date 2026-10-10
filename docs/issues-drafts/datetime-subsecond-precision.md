# Draft issue: Fractional seconds missing from embedded datetime metadata

## Problem

Datetime values can include fractional seconds, but the embedded metadata rewrite path currently formats the EXIF date as whole seconds. The rewritten metadata can therefore lose milliseconds even when the source timestamp includes them.

## Reproduction

Using a synthetic capture timestamp of `2024-06-15T10:00:00.629+11:00`, run the metadata rewrite planner for an asset whose embedded EXIF capture date is absent. Inspect the resulting embedded EXIF datetime.

## Expected

The resulting embedded datetime represents the supplied fractional second (`.629`).

## Actual

The embedded EXIF datetime is `2024:06:15 10:00:00`; the `.629` fraction is absent.
