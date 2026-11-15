//! The sign-in helper of Flatline.
//!
//! The player starts this program when a service asks for a signed-in
//! session. It opens a small window, lets the user sign in, and writes the
//! cookies of the service to the file `yt-dlp` reads.

use std::path::PathBuf;
use std::process::ExitCode;

use media_model::Service;
use web_session::Options;
use web_session_view::run;

/// The arguments the helper needs.
#[derive(Debug)]
struct Arguments {
    /// The service to sign in to.
    service: Service,
    /// The page whose cookies are exported.
    target: String,
    /// The Netscape cookie file to write.
    cookies: PathBuf,
    /// The browser profile kept between runs.
    user_data: PathBuf,
}

/// Reads the arguments of the helper.
fn arguments() -> Result<Arguments, String> {
    let mut service = None;
    let mut target = None;
    let mut cookies = None;
    let mut user_data = None;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = match argument.as_str() {
            "--service" | "--target" | "--cookies" | "--user-data" => {
                arguments.next().ok_or_else(|| format!("{argument} needs a value"))?
            }
            other => return Err(format!("{other} is not a known argument")),
        };
        match argument.as_str() {
            "--service" => {
                service = Some(
                    Service::from_name(&value)
                        .ok_or_else(|| format!("{value} is not a service Flatline knows"))?,
                );
            }
            "--target" => target = Some(value),
            "--cookies" => cookies = Some(PathBuf::from(value)),
            "--user-data" => user_data = Some(PathBuf::from(value)),
            _ => unreachable!("the flags were checked before the value was read"),
        }
    }

    Ok(Arguments {
        service: service.ok_or("--service is missing")?,
        target: target.ok_or("--target is missing")?,
        cookies: cookies.ok_or("--cookies is missing")?,
        user_data: user_data.ok_or("--user-data is missing")?,
    })
}

/// Runs the sign-in and reports the outcome through the exit code.
fn main() -> ExitCode {
    let arguments = match arguments() {
        Ok(arguments) => arguments,
        Err(problem) => {
            eprintln!("flatline-session: {problem}");
            return ExitCode::FAILURE;
        }
    };

    let options = match Options::new(
        arguments.service,
        &arguments.target,
        arguments.cookies,
        arguments.user_data,
    ) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("flatline-session: {error}");
            return ExitCode::FAILURE;
        }
    };

    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("flatline-session: {error}");
            ExitCode::FAILURE
        }
    }
}
