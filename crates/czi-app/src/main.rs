fn main() -> Result<(), Box<dyn std::error::Error>> {
    if czi_ssh::run_embedded_pty_executor_if_requested()? {
        return Ok(());
    }
    if czi_viewer::run_interactive_sftp_bridge_if_requested()? {
        return Ok(());
    }
    czi_viewer::run()?;
    Ok(())
}
