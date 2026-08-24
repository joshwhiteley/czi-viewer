# Demo assets

`czi-viewer-demo.gif` is an original capture of CZI Viewer displaying the project-owned synthetic file produced by:

```sh
cargo run -p czi-core --example generate_demo_czi -- test-data/demo.czi
```

The capture contains no private microscopy data, remote host, account, credential, or source CZI path. The top file-selection bar is excluded from every frame. The GIF uses a slow crop and zoom over the real application window; it does not simulate channel changes or analysis results.

The recording and synthetic input are part of this repository and are available under the repository's `MIT OR Apache-2.0` license.

## Privacy and licensing rules

- Record only project-owned synthetic data or data with written redistribution permission.
- Do not show patient, participant, specimen, account, host, path, SSH, VPN, credential, or license information.
- Remove identifying window titles, notifications, desktop files, and metadata before capture.
- Use original recordings, screenshots, music, and graphics, or assets with a license that permits repository redistribution.
- Record the source, license, creator, and approval in the pull request that adds a media file.
- Do not imply ZEISS endorsement or use ZEISS marks beyond necessary factual text.

## Future extended demo storyboard

Use `cargo run -p czi-core --example generate_demo_czi -- test-data/demo.czi` to create the ignored, synthetic input.

| Time | Action | On-screen result |
| --- | --- | --- |
| 0–2 s | Open `demo.czi`. | Small 2 × 2 mosaic appears. |
| 2–5 s | Pan or zoom out, then select **Blue**. | Tiled mosaic and named channel are clear. |
| 5–8 s | Open **Metadata** or change to **Green**. | Three safe synthetic channel names are visible. |
| 8–11 s | Select **Save PNG**. | The annotated canvas export action is visible. |

Keep any replacement recording short, use a clean local session, and label the file with its source and license in the PR.
