#![warn(unused_crate_dependencies, unreachable_pub)]
#![deny(unused_must_use, rust_2018_idioms)]

use alloy_primitives::{hex, Address, FixedBytes};
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use console::Term;
use fs4::FileExt;
use ocl::{Buffer, Context, Device, MemFlags, Platform, ProQue, Program, Queue};
use rand::{thread_rng, Rng};
use rayon::prelude::*;
use separator::Separatable;
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::time::{SystemTime, UNIX_EPOCH};
use terminal_size::{terminal_size, Height};
use tiny_keccak::{Hasher, Keccak};

mod reward;
pub use reward::Reward;

// workset size (tweak this!)
const WORK_SIZE: u32 = 0x20000000; // max. 0x15400000 to abs. max 0xffffffff - increased for RTX 5070 Ti

const WORK_FACTOR: u128 = (WORK_SIZE as u128) / 1_000_000;
const CONTROL_CHARACTER: u8 = 0xff;
const MAX_INCREMENTER: u64 = 0xffffffffffff;

static KERNEL_SRC: &str = include_str!("./kernels/keccak256.cl");

/// Requires three hex-encoded arguments: the address of the contract that will
/// be calling CREATE2, the address of the caller of said contract *(assuming
/// the contract calling CREATE2 has frontrunning protection in place - if not
/// applicable to your use-case you can set it to the null address)*, and the
/// keccak-256 hash of the bytecode that is provided by the contract calling
/// CREATE2 that will be used to initialize the new contract. An additional set
/// of three optional values may be provided: a device to target for OpenCL GPU
/// search, a threshold for leading zeroes to search for, and a threshold for
/// total zeroes to search for.
pub struct Config {
    pub factory_address: [u8; 20],
    pub calling_address: [u8; 20],
    pub init_code_hash: [u8; 32],
    pub gpu_device: u8,
    pub leading_zeroes_threshold: u8,
    pub total_zeroes_threshold: u8,
}

/// Validate the provided arguments and construct the Config struct.
impl Config {
    pub fn new(mut args: std::env::Args) -> Result<Self, &'static str> {
        // get args, skipping first arg (program name)
        args.next();

        let Some(factory_address_string) = args.next() else {
            return Err("didn't get a factory_address argument");
        };
        let Some(calling_address_string) = args.next() else {
            return Err("didn't get a calling_address argument");
        };
        let Some(init_code_hash_string) = args.next() else {
            return Err("didn't get an init_code_hash argument");
        };

        let gpu_device_string = match args.next() {
            Some(arg) => arg,
            None => String::from("255"), // indicates that CPU will be used.
        };
        let leading_zeroes_threshold_string = match args.next() {
            Some(arg) => arg,
            None => String::from("3"),
        };
        let total_zeroes_threshold_string = match args.next() {
            Some(arg) => arg,
            None => String::from("5"),
        };

        // convert main arguments from hex string to vector of bytes
        let Ok(factory_address_vec) = hex::decode(factory_address_string) else {
            return Err("could not decode factory address argument");
        };
        let Ok(calling_address_vec) = hex::decode(calling_address_string) else {
            return Err("could not decode calling address argument");
        };
        let Ok(init_code_hash_vec) = hex::decode(init_code_hash_string) else {
            return Err("could not decode initialization code hash argument");
        };

        // convert from vector to fixed array
        let Ok(factory_address) = factory_address_vec.try_into() else {
            return Err("invalid length for factory address argument");
        };
        let Ok(calling_address) = calling_address_vec.try_into() else {
            return Err("invalid length for calling address argument");
        };
        let Ok(init_code_hash) = init_code_hash_vec.try_into() else {
            return Err("invalid length for initialization code hash argument");
        };

        // convert gpu arguments to u8 values
        let Ok(gpu_device) = gpu_device_string.parse::<u8>() else {
            return Err("invalid gpu device value");
        };
        let Ok(leading_zeroes_threshold) = leading_zeroes_threshold_string.parse::<u8>() else {
            return Err("invalid leading zeroes threshold value supplied");
        };
        let Ok(total_zeroes_threshold) = total_zeroes_threshold_string.parse::<u8>() else {
            return Err("invalid total zeroes threshold value supplied");
        };

        if leading_zeroes_threshold > 20 {
            return Err("invalid value for leading zeroes threshold argument. (valid: 0..=20)");
        }
        if total_zeroes_threshold > 20 && total_zeroes_threshold != 255 {
            return Err("invalid value for total zeroes threshold argument. (valid: 0..=20 | 255)");
        }

        Ok(Self {
            factory_address,
            calling_address,
            init_code_hash,
            gpu_device,
            leading_zeroes_threshold,
            total_zeroes_threshold,
        })
    }
}

/// Given a Config object with a factory address, a caller address, and a
/// keccak-256 hash of the contract initialization code, search for salts that
/// will enable the factory contract to deploy a contract to a gas-efficient
/// address via CREATE2.
///
/// The 32-byte salt is constructed as follows:
///   - the 20-byte calling address (to prevent frontrunning)
///   - a random 6-byte segment (to prevent collisions with other runs)
///   - a 6-byte nonce segment (incrementally stepped through during the run)
///
/// When a salt that will result in the creation of a gas-efficient contract
/// address is found, it will be appended to `efficient_addresses.txt` along
/// with the resultant address and the "value" (i.e. approximate rarity) of the
/// resultant address.
pub fn cpu(config: Config) -> Result<(), Box<dyn Error>> {
    // (create if necessary) and open a file where found salts will be written
    let file = output_file();

    // create object for computing rewards (relative rarity) for a given address
    let rewards = Reward::new();

    // begin searching for addresses
    loop {
        // header: 0xff ++ factory ++ caller ++ salt_random_segment (47 bytes)
        let mut header = [0; 47];
        header[0] = CONTROL_CHARACTER;
        header[1..21].copy_from_slice(&config.factory_address);
        header[21..41].copy_from_slice(&config.calling_address);
        header[41..].copy_from_slice(&FixedBytes::<6>::random()[..]);

        // create new hash object
        let mut hash_header = Keccak::v256();

        // update hash with header
        hash_header.update(&header);

        // iterate over a 6-byte nonce and compute each address
        (0..MAX_INCREMENTER)
            .into_par_iter() // parallelization
            .for_each(|salt| {
                let salt = salt.to_le_bytes();
                let salt_incremented_segment = &salt[..6];

                // clone the partially-hashed object
                let mut hash = hash_header.clone();

                // update with body and footer (total: 38 bytes)
                hash.update(salt_incremented_segment);
                hash.update(&config.init_code_hash);

                // hash the payload and get the result
                let mut res: [u8; 32] = [0; 32];
                hash.finalize(&mut res);

                // get the address that results from the hash
                let address = <&Address>::try_from(&res[12..]).unwrap();

                // count total and leading zero bytes
                let mut total = 0;
                let mut leading = 21;
                for (i, &b) in address.iter().enumerate() {
                    if b == 0 {
                        total += 1;
                    } else if leading == 21 {
                        // set leading on finding non-zero byte
                        leading = i;
                    }
                }

                // only proceed if there are at least three zero bytes
                if total < 3 {
                    return;
                }

                // look up the reward amount
                let key = leading * 20 + total;
                let reward_amount = rewards.get(&key);

                // only proceed if an efficient address has been found
                if reward_amount.is_none() {
                    return;
                }

                // get the full salt used to create the address
                let header_hex_string = hex::encode(header);
                let body_hex_string = hex::encode(salt_incremented_segment);
                let full_salt = format!("0x{}{}", &header_hex_string[42..], &body_hex_string);

                // display the salt and the address.
                let output = format!(
                    "{full_salt} => {address} => {}",
                    reward_amount.unwrap_or("0")
                );
                println!("{output}");

                // create a lock on the file before writing
                file.lock_exclusive().expect("Couldn't lock file.");

                // write the result to file
                writeln!(&file, "{output}")
                    .expect("Couldn't write to `efficient_addresses.txt` file.");

                // release the file lock
                FileExt::unlock(&file).expect("Couldn't unlock file.");
            });
    }
}

/// Given a Config object with a factory address, a caller address, a keccak-256
/// hash of the contract initialization code, and a device ID, search for salts
/// using OpenCL that will enable the factory contract to deploy a contract to a
/// gas-efficient address via CREATE2. This method also takes threshold values
/// for both leading zero bytes and total zero bytes - any address that does not
/// meet or exceed the threshold will not be returned. Default threshold values
/// are three leading zeroes or five total zeroes.
///
/// The 32-byte salt is constructed as follows:
///   - the 20-byte calling address (to prevent frontrunning)
///   - a random 4-byte segment (to prevent collisions with other runs)
///   - a 4-byte segment unique to each work group running in parallel
///   - a 4-byte nonce segment (incrementally stepped through during the run)
///
/// When a salt that will result in the creation of a gas-efficient contract
/// address is found, it will be appended to `efficient_addresses.txt` along
/// with the resultant address and the "value" (i.e. approximate rarity) of the
/// resultant address.
///
/// This method is still highly experimental and could almost certainly use
/// further optimization - contributions are more than welcome!
pub fn gpu(config: Config) -> ocl::Result<()> {
    println!(
        "Setting up experimental OpenCL miner using device {}...",
        config.gpu_device
    );

    // (create if necessary) and open a file where found salts will be written
    let file = output_file();

    // create object for computing rewards (relative rarity) for a given address
    let rewards = Reward::new();

    // track how many addresses have been found and information about them
    let mut found: u64 = 0;
    let mut found_list: Vec<String> = vec![];

    // set up a controller for terminal output
    let term = Term::stdout();

    // Find NVIDIA platform instead of using default
    let platforms = Platform::list();
    println!("Available OpenCL platforms:");
    for (i, platform) in platforms.iter().enumerate() {
        println!("  Platform {}: {}", i, platform.name().unwrap_or_else(|_| "Unknown".to_string()));
    }
    
    // Try to find NVIDIA platform, fall back to default if not found
    let platform = platforms.iter()
        .find(|p| p.name().unwrap_or_default().contains("NVIDIA"))
        .cloned()
        .unwrap_or_else(|| Platform::new(ocl::core::default_platform().unwrap()));
    
    println!("Selected OpenCL Platform: {}", platform.name().unwrap_or_else(|_| "Unknown".to_string()));

    // List available devices on this platform
    let devices = Device::list_all(platform)?;
    println!("Available devices on selected platform:");
    for (i, device) in devices.iter().enumerate() {
        println!("  Device {}: {}", i, device.name().unwrap_or_else(|_| "Unknown".to_string()));
    }
    
    // set up the device to use
    let device = Device::by_idx_wrap(platform, config.gpu_device as usize)?;
    println!("Selected OpenCL Device: {}", device.name().unwrap_or_else(|_| "Unknown".to_string()));
    let max_wg_size = device.max_wg_size().unwrap_or(256);
    println!("Max Work Group Size: {}", max_wg_size);
    
    // Calculate optimal local work size (typically 256 or 512 for modern GPUs)
    let local_work_size = std::cmp::min(max_wg_size as u32, 512);
    println!("Using Local Work Size: {}", local_work_size);
    
    // Ensure global work size is multiple of local work size
    // Divide by 8 for vectorization (each work item processes 8 nonces)
    let vectorized_work_size = WORK_SIZE / 8;
    let global_work_size = ((vectorized_work_size + local_work_size - 1) / local_work_size) * local_work_size;
    println!("Using Global Work Size: {} (8x vectorized from {})", global_work_size, WORK_SIZE);

    // set up the context to use
    let context = Context::builder()
        .platform(platform)
        .devices(device)
        .build()?;

    // set up the program to use
    let program = Program::builder()
        .devices(device)
        .src(mk_kernel_src(&config))
        .build(&context)?;

    // set up the queue to use
    let queue = Queue::new(&context, device, None)?;

    // set up the "proqueue" (or amalgamation of various elements) to use
    let ocl_pq = ProQue::new(context, queue, program, Some(global_work_size));

    // create a random number generator
    let mut rng = thread_rng();

    // determine the start time
    let start_time: f64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    // set up variables for tracking performance
    let mut rate: f64 = 0.0;
    let mut cumulative_nonce: u64 = 0;

    // the previous timestamp of printing to the terminal
    let mut previous_time: f64 = 0.0;

    // the last work duration in milliseconds
    let mut work_duration_millis: u64 = 0;

    // Create reusable buffers once to avoid memory leaks
    let mut message_buffer = Buffer::builder()
        .queue(ocl_pq.queue().clone())
        .flags(MemFlags::new().read_write())
        .len(4)
        .build()?;

    let mut nonce_buffer = Buffer::builder()
        .queue(ocl_pq.queue().clone())
        .flags(MemFlags::new().read_write())
        .len(1)
        .build()?;

    // Increase solutions buffer size for vectorization (64 slots)
    let mut solutions: Vec<u64> = vec![0; 64];
    let solutions_buffer = Buffer::builder()
        .queue(ocl_pq.queue().clone())
        .flags(MemFlags::new().write_only())
        .len(64)
        .copy_host_slice(&solutions)
        .build()?;

    // begin searching for addresses
    loop {
        // construct the 4-byte message to hash, leaving last 8 of salt empty
        let salt = FixedBytes::<4>::random();

        // Update the message buffer with new salt
        message_buffer.write(&salt[..]).enq()?;

        // reset nonce & create a buffer to view it in little-endian
        // for more uniformly distributed nonces, we shall initialize it to a random value
        let mut nonce: [u32; 1] = rng.gen();
        let mut view_buf = [0; 8];

        // Update the nonce buffer with initial nonce
        nonce_buffer.write(&nonce[..]).enq()?;

        // Clear solutions buffer before starting
        solutions.fill(0);
        solutions_buffer.write(&solutions[..]).enq()?;

        // repeatedly enqueue kernel to search for new addresses
        loop {
            // build the kernel and define the type of each buffer
            let kern = ocl_pq
                .kernel_builder("hashMessage")
                .arg_named("message", None::<&Buffer<u8>>)
                .arg_named("nonce", None::<&Buffer<u32>>)
                .arg_named("solutions", None::<&Buffer<u64>>)
                .build()?;

            // set each buffer
            kern.set_arg("message", Some(&message_buffer))?;
            kern.set_arg("nonce", Some(&nonce_buffer))?;
            kern.set_arg("solutions", &solutions_buffer)?;

            // enqueue the kernel with proper work group sizing
            unsafe { 
                kern.cmd()
                    .global_work_size(global_work_size)
                    .local_work_size(local_work_size)
                    .enq()? 
            };

            // calculate the current time
            let mut now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            let current_time = now.as_secs() as f64;

            // we don't want to print too fast
            let print_output = current_time - previous_time > 0.99;
            previous_time = current_time;

            // clear the terminal screen
            if print_output {
                term.clear_screen()?;

                // get the total runtime and parse into hours : minutes : seconds
                let total_runtime = current_time - start_time;
                let total_runtime_hrs = total_runtime as u64 / 3600;
                let total_runtime_mins = (total_runtime as u64 - total_runtime_hrs * 3600) / 60;
                let total_runtime_secs = total_runtime
                    - (total_runtime_hrs * 3600) as f64
                    - (total_runtime_mins * 60) as f64;

                // determine the number of attempts being made per second
                // Account for 8x vectorization (each work item processes 8 nonces)
                let work_factor = (global_work_size as u128 * 8) / 1_000_000;
                let work_rate: u128 = work_factor * cumulative_nonce as u128;
                if total_runtime > 0.0 {
                    rate = 1.0 / total_runtime;
                }

                // fill the buffer for viewing the properly-formatted nonce
                LittleEndian::write_u64(&mut view_buf, (nonce[0] as u64) << 32);

                // calculate the terminal height, defaulting to a height of ten rows
                let height = terminal_size().map(|(_w, Height(h))| h).unwrap_or(10);

                // display information about the total runtime and work size
                term.write_line(&format!(
                    "total runtime: {}:{:02}:{:02} ({} cycles)\t\t\t\
                     work size per cycle: {} (8x vectorized)",
                    total_runtime_hrs,
                    total_runtime_mins,
                    total_runtime_secs,
                    cumulative_nonce,
                    (global_work_size * 8).separated_string(),
                ))?;

                // display information about the attempt rate and found solutions
                term.write_line(&format!(
                    "rate: {:.2} million attempts per second\t\t\t\
                     total found this run: {}",
                    work_rate as f64 * rate,
                    found
                ))?;

                // display information about the current search criteria
                term.write_line(&format!(
                    "current search space: {}xxxxxxxx{:08x}\t\t\
                     threshold: {} leading or {} total zeroes",
                    hex::encode(salt),
                    BigEndian::read_u64(&view_buf),
                    config.leading_zeroes_threshold,
                    config.total_zeroes_threshold
                ))?;

                // display recently found solutions based on terminal height
                let rows = if height < 5 { 1 } else { height as usize - 4 };
                let last_rows: Vec<String> = found_list.iter().cloned().rev().take(rows).collect();
                let ordered: Vec<String> = last_rows.iter().cloned().rev().collect();
                let recently_found = &ordered.join("\n");
                term.write_line(recently_found)?;
            }

            // increment the cumulative nonce (does not reset after a match)
            cumulative_nonce += 1;

            // record the start time of the work
            let work_start_time_millis = now.as_secs() * 1000 + now.subsec_nanos() as u64 / 1000000;

            // sleep for 98% of the previous work duration to conserve CPU
            if work_duration_millis != 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    work_duration_millis * 980 / 1000,
                ));
            }

            // read the solutions from the device
            solutions_buffer.read(&mut solutions).enq()?;

            // record the end time of the work and compute how long the work took
            now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            work_duration_millis = (now.as_secs() * 1000 + now.subsec_nanos() as u64 / 1000000)
                - work_start_time_millis;

            // if at least one solution is found, end the loop
            if solutions.iter().any(|&x| x != 0) {
                break;
            }

            // if no solution has yet been found, increment the nonce
            nonce[0] += 1;

            // update the nonce buffer with the incremented nonce value
            nonce_buffer.write(&nonce[..]).enq()?;
        }

        // iterate over each solution, first converting to a fixed array
        for &solution in &solutions {
            if solution == 0 {
                continue;
            }

            let solution = solution.to_le_bytes();

            let mut solution_message = [0; 85];
            solution_message[0] = CONTROL_CHARACTER;
            solution_message[1..21].copy_from_slice(&config.factory_address);
            solution_message[21..41].copy_from_slice(&config.calling_address);
            solution_message[41..45].copy_from_slice(&salt[..]);
            solution_message[45..53].copy_from_slice(&solution);
            solution_message[53..].copy_from_slice(&config.init_code_hash);

            // create new hash object
            let mut hash = Keccak::v256();

            // update with header
            hash.update(&solution_message);

            // hash the payload and get the result
            let mut res: [u8; 32] = [0; 32];
            hash.finalize(&mut res);

            // get the address that results from the hash
            let address = <&Address>::try_from(&res[12..]).unwrap();

            // count total and leading zero bytes
            let mut total = 0;
            let mut leading = 21;
            for (i, &b) in address.iter().enumerate() {
                if b == 0 {
                    total += 1;
                } else if leading == 21 {
                    // set leading on finding non-zero byte
                    leading = i;
                }
            }

            let key = leading * 20 + total;
            let reward = rewards.get(&key).unwrap_or("0");
            let output = format!(
                "0x{}{}{} => {} => {}",
                hex::encode(config.calling_address),
                hex::encode(salt),
                hex::encode(solution),
                address,
                reward,
            );

            let show = format!("{output} ({leading} / {total})");
            found_list.push(show.to_string());

            file.lock_exclusive().expect("Couldn't lock file.");

            writeln!(&file, "{output}").expect("Couldn't write to `efficient_addresses.txt` file.");

            FileExt::unlock(&file).expect("Couldn't unlock file.");
            found += 1;
        }
    }
}

#[track_caller]
fn output_file() -> File {
    OpenOptions::new()
        .append(true)
        .create(true)
        .read(true)
        .open("efficient_addresses.txt")
        .expect("Could not create or open `efficient_addresses.txt` file.")
}

/// Creates the OpenCL kernel source code by populating the template with the
/// values from the Config object.
fn mk_kernel_src(config: &Config) -> String {
    let mut src = String::with_capacity(2048 + KERNEL_SRC.len());

    let factory = config.factory_address.iter();
    let caller = config.calling_address.iter();
    let hash = config.init_code_hash.iter();
    let hash = hash.enumerate().map(|(i, x)| (i + 52, x));
    for (i, x) in factory.chain(caller).enumerate().chain(hash) {
        writeln!(src, "#define S_{} {}u", i + 1, x).unwrap();
    }
    let lz = config.leading_zeroes_threshold;
    writeln!(src, "#define LEADING_ZEROES {lz}").unwrap();
    let tz = config.total_zeroes_threshold;
    writeln!(src, "#define TOTAL_ZEROES {tz}").unwrap();

    src.push_str(KERNEL_SRC);

    src
}
