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

//! This module contains common functionality related to mining protocol client and allows
//! executing a specific type of mining protocol client instance.

mod scheduler;

// Sub-modules with client implementation
pub mod stratum_v2;

use crate::error;
use crate::job;
use crate::node;
use crate::stats;
use crate::work;

// Scheduler re-exports
pub use scheduler::JobExecutor;

use bosminer_config::client::{Descriptor, Protocol};

use futures::channel::mpsc;
use futures::lock::Mutex;
use ii_async_compat::futures;

use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub struct Handle {
    // Basic information about client used for connection to remote server
    pub descriptor: Descriptor,
    node: Arc<dyn node::Client>,
    enabled: AtomicBool,
    engine_sender: Arc<work::EngineSender>,
    solution_sender: mpsc::UnboundedSender<work::Solution>,
}

impl Handle {
    pub fn replace_engine_generator(
        &self,
        engine_generator: work::EngineGenerator,
    ) -> work::EngineGenerator {
        self.engine_sender
            .replace_engine_generator(engine_generator)
    }

    /// Tests if solution should be delivered to this client
    /// NOTE: This comparison uses trait method `node::Info::get_unique_ptr` to unify dynamic
    /// objects to point to the same pointer otherwise direct comparison of self with other is never
    /// satisfied even if the dynamic objects are same.
    pub fn matching_solution(&self, solution: &work::Solution) -> bool {
        Arc::ptr_eq(
            &self.node.clone().get_unique_ptr(),
            &solution.origin().get_unique_ptr(),
        )
    }

    #[inline]
    pub fn is_running(&self) -> bool {
        self.is_enabled() && self.status() == crate::sync::Status::Running
    }

    #[inline]
    pub fn status(&self) -> crate::sync::Status {
        self.node.status().status()
    }

    #[inline]
    fn start(&self) {
        if self.node.status().initiate_starting() {
            // The client can be started safely
            self.node.clone().start();
        }
    }

    #[inline]
    fn stop(&self) {
        if self.node.status().initiate_stopping() {
            // The client can be stopped safely
            self.node.clone().stop();
        }
    }

    /// Check if current state of the client is enabled
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Try to enable the client. Default client state should be disabled.
    pub(crate) fn try_enable(&self) -> Result<(), ()> {
        let was_enabled = self.enabled.swap(true, Ordering::Relaxed);
        if !was_enabled {
            // Immediately start the client when it was disabled
            // TODO: force the scheduler
            self.start();
            Ok(())
        } else {
            Err(())
        }
    }

    /// Try to disable the client
    pub(crate) fn try_disable(&self) -> Result<(), ()> {
        let was_enabled = self.enabled.swap(false, Ordering::Relaxed);
        if was_enabled {
            // Immediately stop the client when it was disabled
            // TODO: force the scheduler
            self.stop();
            Ok(())
        } else {
            Err(())
        }
    }

    #[inline]
    pub(crate) fn stats(&self) -> &dyn stats::Client {
        self.node.client_stats()
    }

    #[inline]
    pub(crate) async fn get_last_job(&self) -> Option<Arc<dyn job::Bitcoin>> {
        self.node.get_last_job().await
    }
}

impl From<Descriptor> for Handle {
    fn from(descriptor: Descriptor) -> Self {
        let (solution_sender, solution_receiver) = mpsc::unbounded();
        // Initially register new client without ability to send work
        let engine_sender = Arc::new(work::EngineSender::new(None));

        let job_solver = job::Solver::new(engine_sender.clone(), solution_receiver);
        let client_node = match &descriptor.protocol {
            Protocol::StratumV2 => stratum_v2::StratumClient::new(
                stratum_v2::ConnectionDetails::from_descriptor(&descriptor),
                job_solver,
            ),
        };

        Self {
            descriptor,
            node: Arc::new(client_node),
            enabled: AtomicBool::new(false),
            engine_sender,
            solution_sender,
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.node.stop();
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Handle) -> bool {
        Arc::ptr_eq(
            &self.node.clone().get_unique_ptr(),
            &other.node.clone().get_unique_ptr(),
        )
    }
}

#[derive(Debug)]
pub struct Group {
    scheduler_client_handles: Mutex<Vec<scheduler::ClientHandle>>,
    midstate_count: usize,
}

impl Group {
    #[inline]
    pub async fn count(&self) -> usize {
        self.scheduler_client_handles.lock().await.len()
    }

    #[inline]
    pub async fn is_empty(&self) -> bool {
        self.scheduler_client_handles.lock().await.is_empty()
    }

    #[inline]
    pub async fn get_clients(&self) -> Vec<Arc<Handle>> {
        self.scheduler_client_handles
            .lock()
            .await
            .iter()
            .map(|scheduler_handle| scheduler_handle.client_handle.clone())
            .collect()
    }

    pub async fn add_client(&self, client_handle: Handle) -> Arc<Handle> {
        let midstate_count = self.midstate_count;
        let _ = client_handle.replace_engine_generator(Box::new(move |job| {
            Arc::new(work::engine::VersionRolling::new(job, midstate_count))
        }));
        let _ = client_handle.try_disable();

        let client_handle = Arc::new(client_handle);
        let scheduler_client_handle = scheduler::ClientHandle::new(client_handle.clone());
        self.scheduler_client_handles
            .lock()
            .await
            .push(scheduler_client_handle);

        if client_handle.descriptor.enable {
            client_handle
                .try_enable()
                .expect("BUG: client is already enabled");
        }

        client_handle
    }

    pub async fn remove_client_at(&self, index: usize) -> Result<Arc<Handle>, error::Client> {
        let mut scheduler_client_handles = self.scheduler_client_handles.lock().await;
        if index >= scheduler_client_handles.len() {
            Err(error::Client::Missing)
        } else {
            let client_handle = scheduler_client_handles.remove(index).client_handle;
            // Immediately disable client to force scheduler to select another client
            let _ = client_handle.try_disable();
            Ok(client_handle)
        }
    }

    pub async fn move_client_to(
        &self,
        index_from: usize,
        index_to: usize,
    ) -> Result<Arc<Handle>, error::Client> {
        let mut scheduler_client_handles = self.scheduler_client_handles.lock().await;
        let len = scheduler_client_handles.len();
        if index_from >= len || index_to >= len {
            return Err(error::Client::Missing);
        }

        if index_from > index_to {
            *scheduler_client_handles = [
                &scheduler_client_handles[0..index_to],
                &scheduler_client_handles[index_from..index_from + 1],
                &scheduler_client_handles[index_to..index_from],
                &scheduler_client_handles[index_from + 1..],
            ]
            .concat();
        } else if index_from < index_to {
            *scheduler_client_handles = [
                &scheduler_client_handles[0..index_from],
                &scheduler_client_handles[index_from + 1..index_to + 1],
                &scheduler_client_handles[index_from..index_from + 1],
                &scheduler_client_handles[index_to + 1..],
            ]
            .concat();
        }

        Ok(scheduler_client_handles[index_to].client_handle.clone())
    }
}

/// Keeps track of all active clients
pub struct Registry {
    list: Vec<scheduler::Handle>,
}

impl Registry {
    pub fn new() -> Self {
        Self { list: vec![] }
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.list.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    #[inline]
    fn iter(&self) -> slice::Iter<scheduler::Handle> {
        self.list.iter()
    }

    #[inline]
    fn iter_mut(&mut self) -> slice::IterMut<scheduler::Handle> {
        self.list.iter_mut()
    }

    pub fn get_clients(&self) -> Vec<Arc<Handle>> {
        self.list
            .iter()
            .map(|scheduler_handle| scheduler_handle.client_handle.clone())
            .collect()
    }

    #[inline]
    fn get_scheduler_handle(
        &self,
        client_handle: &Arc<Handle>,
    ) -> Result<&scheduler::Handle, error::Client> {
        self.list
            .iter()
            .find(|scheduler_handle| scheduler_handle.client_handle == *client_handle)
            .ok_or(error::Client::Missing)
    }

    fn recalculate_quotas(&mut self, reset_generated_work: bool) {
        let clients = self.count();
        let percentage_share = if clients > 0 {
            1.0 / clients as f64
        } else {
            return;
        };

        // Update all clients with newly calculated percentage share.
        // Also reset generated work to prevent switching all future work to new client because
        // new client has zero shares and so maximal error.
        for mut scheduler_handle in self.iter_mut() {
            if reset_generated_work {
                scheduler_handle.reset_generated_work();
            }
            scheduler_handle.percentage_share = percentage_share;
        }
    }

    /// Register client that implements a protocol set in `descriptor`
    fn register_client(&mut self, client_handle: Arc<Handle>) -> &scheduler::Handle {
        self.list.push(scheduler::Handle::new(client_handle));

        self.recalculate_quotas(true);
        self.list.last().expect("BUG: client list is empty")
    }

    fn unregister_client(
        &mut self,
        client_handle: Arc<Handle>,
    ) -> Result<scheduler::Handle, error::Client> {
        if let Some(index) = self
            .list
            .iter()
            .position(|scheduler_handle| scheduler_handle.client_handle == client_handle)
        {
            let scheduler_handle = self.list.remove(index);
            self.recalculate_quotas(false);

            Ok(scheduler_handle)
        } else {
            Err(error::Client::Missing)
        }
    }

    fn reorder_clients<'a, 'b, T>(&'a mut self, client_handles: T) -> Result<(), error::Client>
    where
        T: Iterator<Item = &'b Arc<Handle>>,
    {
        let mut scheduler_handles = Vec::with_capacity(self.list.len());
        for client_handle in client_handles {
            scheduler_handles.push(self.get_scheduler_handle(&client_handle)?.clone());
        }
        if self.list.len() != scheduler_handles.len() {
            Err(error::Client::Additional)
        } else {
            self.list = scheduler_handles;
            Ok(())
        }
    }
}
