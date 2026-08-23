# Demo assets

`hero-placeholder.svg` is temporary neutral artwork. It is clearly labeled as a placeholder and is not an application screenshot. Replace it with a real demo recording when one is approved.

## Privacy and licensing rules

- Record only project-owned synthetic data or data with written redistribution permission.
- Do not show patient, participant, specimen, account, host, path, SSH, VPN, credential, or license information.
- Remove identifying window titles, notifications, desktop files, and metadata before capture.
- Use original recordings, screenshots, music, and graphics, or assets with a license that permits repository redistribution.
- Record the source, license, creator, and approval in the pull request that adds a media file.
- Do not imply ZEISS endorsement or use ZEISS marks beyond necessary factual text.

## 8–12 second demo storyboard

Use `cargo run -p czi-core --example generate_demo_czi -- test-data/demo.czi` to create the ignored, synthetic input.

| Time | Action | On-screen result |
| --- | --- | --- |
| 0–2 s | Open `demo.czi`. | Small 2 × 2 mosaic appears. |
| 2–5 s | Pan or zoom out, then select **Blue**. | Tiled mosaic and named channel are clear. |
| 5–8 s | Open **Metadata** or change to **Green**. | Three safe synthetic channel names are visible. |
| 8–11 s | Select **Save PNG**. | The annotated canvas export action is visible. |

Keep the recording short, use a clean local session, and label the file with its source and license in the PR.
