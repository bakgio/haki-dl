use std::error::Error;

use haki_dl::{combine_files, partial_combine_files};

mod support;
use support::TempDirectory;

#[tokio::test]
async fn binary_and_partial_combine_write_temp_outputs_and_remove_grouped_inputs()
-> Result<(), Box<dyn Error>> {
    let temp = TempDirectory::new("mux-combine")?;
    let first = temp.path().join("0.ts");
    let second = temp.path().join("1.ts");
    std::fs::write(&first, b"aa")?;
    std::fs::write(&second, b"bb")?;
    let output = temp.path().join("joined.ts");

    combine_files(&[first.clone(), second.clone()], &output).await?;
    assert_eq!(std::fs::read(&output)?, b"aabb");

    let partials = partial_combine_files(&[first.clone(), second.clone()]).await?;
    assert_eq!(partials.len(), 1);
    assert_eq!(std::fs::read(&partials[0])?, b"aabb");
    assert!(!first.exists());
    assert!(!second.exists());
    Ok(())
}
