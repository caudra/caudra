fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    caudra_models::cli::run()
}
