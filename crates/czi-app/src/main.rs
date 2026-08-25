fn main() -> Result<(), Box<dyn std::error::Error>> {
    if czi_viewer::run_update_helper_if_requested()? {
        return Ok(());
    }
    if czi_viewer::run_anyconnect_vpn_executor_if_requested()? {
        return Ok(());
    }
    if czi_ssh::run_embedded_pty_executor_if_requested()? {
        return Ok(());
    }
    czi_viewer::run()?;
    Ok(())
}
