use anyhow::Result;
use clap::Parser;
use ppk2::{
    measurement::MeasurementMatch,
    try_find_ppk2_port,
    types::{DevicePower, LogicPortPins, MeasurementMode, SourceVoltage},
    Ppk2,
};
use std::{
    sync::mpsc::RecvTimeoutError,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, Level as LogLevel};
use tracing_subscriber::FmtSubscriber;

use serde::{Deserialize, Serialize};

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
    #[clap(
        env,
        short = 'f',
        long,
        help = "The csv file the data will be written to, when no file is specified the program does nothing"
    )]
    file: String,
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

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct Sample {
    #[serde(rename = "timestamp (μs)")]
    timestamp: u128,
    #[serde(rename = "power (μA)")]
    power: f32,
    #[serde(rename = "pins (D0-D7)")]
    pins: LogicPortPins,
}

fn write_to_file(ppk2: Ppk2, args: &Args, duration: Duration) -> Result<Ppk2> {
    let mut wtr = csv::Writer::from_path(args.file.clone())?;
    let (rx, kill) = ppk2.start_measurement(args.sps)?;

    info!(
        "Started measurement of {} seconds to \'{:}\'",
        duration.as_secs(),
        args.file
    );

    let start = Instant::now();

    loop {
        let now = Instant::now().duration_since(start);
        if now > duration {
            break Ok(kill()?);
        }

        use MeasurementMatch::*;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Match(m)) => {
                wtr.serialize(Sample {
                    timestamp: now.as_micros(),
                    power: m.micro_amps,
                    pins: m.pins,
                })?;
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

fn wait_for_power(ppk2: Ppk2) -> Result<Ppk2> {
    let (rx, kill) = ppk2.start_measurement(100)?;
    info!("Waiting for power...");

    loop {
        use MeasurementMatch::*;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Match(m)) if m.micro_amps > 0.0 => {
                info!("Power detected!");
                break Ok(kill()?);
            }
            Ok(_) => continue,
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

    let mut ppk2 = configure_ppk2(args).expect("Something went wrong configuring the ppk2:\n");

    ppk2 = wait_for_power(ppk2).expect("Something went wrong waiting for power");
    ppk2 = write_to_file(ppk2, args, Duration::from_secs(5))
        .expect("Something went wrong in measurement:\n");

    info!("Goodbye!");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Sample;
    use csv::{Reader, Writer};
    use ppk2::types::LogicPortPins;

    #[test]
    pub fn dummy_data_test() {
        let mut wtr = Writer::from_path("bar.csv").unwrap();
        let pins0 = LogicPortPins::from(255 as u8);
        let pins1 = pins0.set_level(0, ppk2::types::Level::Either);
        let pins2 = pins0.set_level(7, ppk2::types::Level::Low);

        let samples: Vec<Sample> = vec![
            Sample {
                timestamp: 0,
                power: 1.2,
                pins: pins0,
            },
            Sample {
                timestamp: 1,
                power: 3.4,
                pins: pins1,
            },
            Sample {
                timestamp: 2,
                power: 5.6,
                pins: pins2,
            },
        ];
        for sample in &samples {
            wtr.serialize(sample).unwrap();
        }
        wtr.flush().unwrap();

        let mut rdr = Reader::from_path("bar.csv").unwrap();
        let samples_cmp: Vec<Sample> = rdr.deserialize::<Sample>().map(|mx| mx.unwrap()).collect();
        assert_eq!(samples, samples_cmp);
    }
}
