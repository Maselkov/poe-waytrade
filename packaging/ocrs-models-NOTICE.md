# OCR models

`/usr/share/poe-waytrade/ocrs/text-detection.onnx` and
`text-recognition.onnx` are the pre-trained models of ocrs, which the price
daemon uses to read item displays off the screen:

> Ocrs models (https://github.com/robertknight/ocrs-models), trained on the
> HierText dataset by Google Research. Licensed under CC BY-SA 4.0
> (https://creativecommons.org/licenses/by-sa/4.0/).

They are shipped unmodified, as downloaded from
https://ocrs-models.s3-accelerate.amazonaws.com/. The rest of Waytrade is
GPL-3.0-or-later; the ocrs library itself is MIT or Apache-2.0.
