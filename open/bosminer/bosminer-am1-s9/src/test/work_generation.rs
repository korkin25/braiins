// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use ii_logging::macros::*;

use super::*;

use bosminer::work;

use std::time::Duration;

use std::sync::Arc;

use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::stream::StreamExt;

use ii_async_compat::{sleep, timeout_future, TimeoutResult};

/// Our local abbreviation
type HashChain = crate::HashChain<power::SharedBackend<power::I2cBackend>>;

/// Prepares sample work with empty midstates
/// NOTE: this work has 2 valid nonces:
/// - 0x83ea0372 (solution 0)
/// - 0x09f86be1 (solution 1)
fn prepare_test_work(midstate_count: usize) -> work::Assignment {
    let time = 0xffffffff;
    let job = Arc::new(null_work::NullJob::new(time, 0xffff_ffff, 0));

    let one_midstate = work::Midstate {
        version: 0,
        state: [0u8; 32].into(),
    };
    work::Assignment::new(job, vec![one_midstate; midstate_count], time)
}

/// Task that receives solutions from hardware and sends them to channel
async fn receiver_task(
    hash_chain: Arc<Mutex<HashChain>>,
    solution_sender: mpsc::UnboundedSender<work::Solution>,
) {
    let mut rx_io = await!(hash_chain.lock())
        .work_rx_io
        .take()
        .expect("work-rx fifo missing");

    loop {
        let (rx_io_out, solution) = await!(rx_io.recv_solution()).expect("recv solution");
        rx_io = rx_io_out;

        solution_sender
            .unbounded_send(solution)
            .expect("solution send failed");
    }
}

/// Task that receives work from channel and sends it to HW
async fn sender_task(
    hash_chain: Arc<Mutex<HashChain>>,
    mut work_receiver: mpsc::UnboundedReceiver<work::Assignment>,
) {
    let mut tx_io = await!(hash_chain.lock())
        .work_tx_io
        .take()
        .expect("work-tx fifo missing");
    let mut work_registry = registry::WorkRegistry::new(tx_io.work_id_limit());

    loop {
        await!(tx_io.wait_for_room()).expect("wait for tx room");
        let work = await!(work_receiver.next()).expect("failed receiving work");
        let work_id = work_registry.store_work(work.clone());
        // send work is synchronous
        tx_io.send_work(&work, work_id).expect("send work");
    }
}

async fn send_and_receive_test_workloads<'a>(
    work_sender: &'a mpsc::UnboundedSender<work::Assignment>,
    solution_receiver: &'a mut mpsc::UnboundedReceiver<work::Solution>,
    n_send: usize,
    expected_solution_count: usize,
) {
    info!(
        "Sending {} work items and trying to receive {} solutions",
        n_send, expected_solution_count,
    );
    //
    // Put in some tasks
    for _ in 0..n_send {
        let work = prepare_test_work(1);
        work_sender.unbounded_send(work).expect("work send failed");
        // wait time to send out work + to compute work
        // TODO: come up with a formula instead of fixed time interval
        // wait = work_time * number_of_chips + time_to_send_out_a_jov

        await!(sleep(Duration::from_millis(100)));
    }
    let mut returned_solution_count = 0;
    loop {
        match await!(timeout_future(
            solution_receiver.next(),
            Duration::from_millis(1000)
        )) {
            TimeoutResult::TimedOut => break,
            TimeoutResult::Error => panic!("timeout error"),
            TimeoutResult::Returned(_solution) => returned_solution_count += 1,
        }
    }
    assert_eq!(
        returned_solution_count, expected_solution_count,
        "expected {} solutions but got {}",
        expected_solution_count, returned_solution_count
    );
}

async fn start_hchain() -> HashChain {
    let gpio_mgr = gpio::ControlPinManager::new();
    let voltage_ctrl_backend = power::I2cBackend::new(0);
    let voltage_ctrl_backend = power::SharedBackend::new(voltage_ctrl_backend);

    let mut hash_chain = crate::HashChain::new(
        &gpio_mgr,
        voltage_ctrl_backend.clone(),
        config::S9_HASHBOARD_INDEX,
        crate::MidstateCount::new(1),
        1,
    )
    .unwrap();
    await!(hash_chain.init()).expect("h_chain init failed");
    hash_chain
}

/// Verifies work generation for a hash chain
///
/// The test runs two batches of work:
/// - the first 3 work items are for initializing input queues of the chips and don't provide any
/// solutions
/// - the next 2 work items yield actual solutions. Since we don't push more work items, the
/// solution 1 never appears on the bus and leave chips output queues. This is fine as this test
/// is intended for initial check of correct operation
#[test]
fn test_work_generation() {
    // Create channels
    let (solution_sender, mut solution_receiver) = mpsc::unbounded();
    let (work_sender, work_receiver) = mpsc::unbounded();

    // Guard lives until the end of the block
    let _work_sender_guard = work_sender.clone();
    let _solution_sender_guard = solution_sender.clone();

    ii_async_compat::run_main_exits(async move {
        // Start HW
        let hash_chain = Arc::new(Mutex::new(await!(start_hchain())));

        // start HW receiver
        ii_async_compat::spawn(receiver_task(hash_chain.clone(), solution_sender));

        // start HW sender
        ii_async_compat::spawn(sender_task(hash_chain.clone(), work_receiver));

        // the first 3 work loads don't produce any solutions, these are merely to initialize the input
        // queue of each hashing chip
        await!(send_and_receive_test_workloads(
            &work_sender,
            &mut solution_receiver,
            3,
            0
        ));

        // submit 2 more work items, since we are intentionally being slow all chips should send a
        // solution for the submitted work
        let more_work_count = 2usize;
        let chip_count = await!(hash_chain.lock()).get_chip_count();
        let expected_solution_count = more_work_count * chip_count;

        await!(send_and_receive_test_workloads(
            &work_sender,
            &mut solution_receiver,
            more_work_count,
            expected_solution_count,
        ));
    });
}
