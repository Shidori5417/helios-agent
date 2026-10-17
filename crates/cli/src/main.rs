// The compatibility `agent-cli` entry point. The implementation lives in the
// library so both executables share one parser and one set of tests.
fn main() -> std::process::ExitCode {
    agent_cli::main()
}
