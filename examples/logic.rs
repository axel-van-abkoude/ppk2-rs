use anyhow::Result;
use clap::Parser;
use ppk2::{
    measurement::MeasurementMatch,
    try_find_ppk2_port,
    types::{DevicePower, MeasurementMode, SourceVoltage},
    Ppk2,
};

use std::{
    sync::mpsc::RecvTimeoutError,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, Level as LogLevel};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser)]
struct Args {
    #[clap(
        env,
        short = 'p',
        long,
        help = "The serial port the PPK2 is connected to. If unspecified, will try to find the PPK2 automatically"
    )]
    serial_port: Option<String>,

    #[clap(
        env,
        short = 'v',
        long,
        help = "The voltage of the device source in mV",
        default_value = "0"
    )]
    voltage: SourceVoltage,

    #[clap(
        env,
        short = 'e',
        long,
        help = "Enable power",
        default_value = "disabled"
    )]
    power: DevicePower,

    #[clap(
        env,
        short = 'm',
        long,
        help = "Measurement mode",
        default_value = "source"
    )]
    mode: MeasurementMode,

    #[clap(env, short = 'l', long, help = "The log level", default_value = "info")]
    log_level: LogLevel,

    #[clap(
        env,
        short = 's',
        long,
        help = "The maximum number of samples to be taken per second. Uses averaging of device samples Samples are analyzed in chunks, and as such the actual number of samples per second will deviate",
        default_value = "100"
    )]
    sps: usize,
}

fn configure_ppk2(args: &Args) -> Result<Ppk2> {
    let ppk2_port = match &args.serial_port {
        Some(p) => p,
        None => &try_find_ppk2_port()?,
    };

    // Connect to PPK2 and initialize
    let mut ppk2 = Ppk2::new(ppk2_port, args.mode)?;
    ppk2.set_source_voltage(args.voltage)?;
    ppk2.set_device_power(args.power)?;

    Ok(ppk2)
}

fn main_loop(
    ppk2: Ppk2,
    args: &Args,
    duration: Duration,
    atot_pin_1: &mut f32,
    atot_pin_2: &mut f32,
) -> Result<Ppk2> {
    // Start measuring.
    let (rx, kill) = ppk2.start_measurement(args.sps)?;

    let start = Instant::now();
    loop {
        let rcv_res = rx.recv_timeout(Duration::from_secs(2));
        let now = Instant::now().duration_since(start);
        if now > duration {
            break Ok(kill()?);
        }
        use MeasurementMatch::*;
        match rcv_res {
            Ok(Match(m)) => {
                debug!("avg: {:.4} μA\t pins: {:?}", m.micro_amps, m.pins.inner());

                if m.pins.pin_is_high(0) {
                    *atot_pin_1 += m.micro_amps;
                }
                if m.pins.pin_is_high(1) {
                    *atot_pin_2 += m.micro_amps;
                }
            }
            Ok(NoMatch) => {
                debug!("No match in the last chunk of measurements");
            }
            Err(RecvTimeoutError::Disconnected) => break Ok(kill()?),
            Err(e) => {
                error!("Error receiving data: {e:?}");
                break Err(e)?;
            }
        }
    }
}

fn main() -> Result<()> {
    let args = &Args::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(args.log_level)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let mut atot_pin_1: f32 = 0.0;
    let mut atot_pin_2: f32 = 0.0;

    let mut ppk2 = configure_ppk2(args).expect("bla");

    ppk2 = main_loop(
        ppk2,
        args,
        Duration::from_secs(5),
        &mut atot_pin_1,
        &mut atot_pin_2,
    )
    .expect("Something went wrong in measurement 1:\n");

    info!("Section total pin[1] = high: {:.4} A", atot_pin_1);
    info!("Section total pin[2] = high: {:.4} A", atot_pin_2);

    ppk2 = main_loop(
        ppk2,
        args,
        Duration::from_secs(5),
        &mut atot_pin_1,
        &mut atot_pin_2,
    )
    .expect("Something went wrong in measurement 2:\n");

    info!("Section total pin[1] = high: {:.4} A", atot_pin_1);
    info!("Section total pin[2] = high: {:.4} A", atot_pin_2);

    info!("Stopping measurements and resetting");
    info!("Goodbye!");
    Ok(())
}

#[cfg(test)]
mod tests {
    //WARNING: only run sequentially (with --test-threads=1)
    use crate::{configure_ppk2, main_loop, Args};
    use clap::Parser;
    use std::thread;
    use std::time::Duration;

    #[test]
    pub fn pin_1_lt_pin_2_ampere() {
        let args = &Args::try_parse_from(["dummy", "-e", "enabled", "-m", "ampere"]).unwrap();

        let mut atot_pin_1: f32 = 0.0;
        let mut atot_pin_2: f32 = 0.0;

        let mut ppk2 = configure_ppk2(args).expect("bla");

        ppk2 = main_loop(
            ppk2,
            args,
            Duration::from_secs(5),
            &mut atot_pin_1,
            &mut atot_pin_2,
        )
        .expect("Something went wrong in measurement 1:\n");
        ppk2.reset()
            .expect("Something went wrong while resetting:\n");
        // give time to close channel TODO: make more robust
        thread::sleep(Duration::from_secs(1));

        assert!(
            atot_pin_1 < atot_pin_2,
            "Total pin 1 high is greater total pin 2 high."
        );
    }

    #[test]
    pub fn pin_1_gt_pin_2_ampere() {
        let args = &Args::try_parse_from(["dummy", "-e", "enabled", "-m", "ampere"]).unwrap();

        let mut atot_pin_1: f32 = 0.0;
        let mut atot_pin_2: f32 = 0.0;

        let mut ppk2 = configure_ppk2(args).expect("bla");

        ppk2 = main_loop(
            ppk2,
            args,
            Duration::from_secs(5),
            &mut atot_pin_1,
            &mut atot_pin_2,
        )
        .expect("Something went wrong in measurement 2:\n");
        ppk2.reset()
            .expect("Something went wrong while resetting:\n");
        // give time to close channel TODO: make more robust
        thread::sleep(Duration::from_secs(1));

        assert!(
            atot_pin_1 > atot_pin_2,
            "Total pin 1 high is less than total pin 2 high."
        );
    }
}
