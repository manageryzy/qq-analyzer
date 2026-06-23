# TXData Notes

## Verified Codec Anchors

`Common.dll` contains the TXData codec paths used by PCQQ:

- `Util::Data::GetTXDataStr`
- `Util::Data::CopyTXDataField`
- `CTXDataField::get_string`
- `CCmdCodecBase::CodeTXData`
- `CCmdCodecBase::DecodeBuffer`
- string, buffer, number, and array decode helpers.

Important verified behavior: a TXData string value is decoded as a complete
field value. The parser must not use broad sliding text discovery inside
`header=8` string values to produce visible text.

## Implementation Direction

The Rust `txdata_codec` module should be the shared implementation for:

- Msg3 `Info` sender/receiver fields.
- InfoStorage decoded field values.
- Nested TXData found in Msg3 elements.
- Diagnostics that need field names, values, text candidates, and numeric
  interpretation.

Any visible rendering rule based on TXData must name the field and its verified
behavior. Unrecognized TXData may be shown in diagnostics but should not become
body text.

