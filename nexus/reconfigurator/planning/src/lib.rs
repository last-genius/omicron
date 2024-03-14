// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # Omicron deployment management
//!
//! **This system is still under development.  Some of what's below is
//! more aspirational than real.**
//!
//! ## Overview
//!
//! "Deployment management" here refers broadly to managing the lifecycle of
//! software components.  That includes deployment, undeployment, upgrade,
//! bringing into service, and removing from service.  It includes
//! dynamically-deployed components (like most Omicron zones, like Nexus and
//! CockroachDB) as well as components that are tied to fixed physical hardware
//! (like the host operating system and device firmware).  This system will
//! potentially manage configuration, too.  See RFD 418 for background and a
//! survey of considerations here.
//!
//! The basic idea is that you have:
//!
//! * **fleet policy**: describes things like how many CockroachDB nodes there
//!   should be, how many Nexus nodes there should be, the target system version
//!   that all software should be running, which sleds are currently in service,
//!   etc.
//!
//! * **inventory \[collections\]**: describe what software is currently
//!   running on which hardware components, including versions and
//!   configuration.  This includes all control-plane-managed software,
//!   including device firmware, host operating system, Omicron zones, etc.
//!
//! * **\[deployment\] blueprints**: describe what software _should_ be running
//!   on which hardware components, including versions and configuration.  Like
//!   inventory collections, the plan covers all control-plane-managed software
//!   and configuration.  Plans must be specific enough that multiple Nexus
//!   instances can attempt to realize the same blueprint concurrently without
//!   stomping on each other.  (For example, it's not specific enough to say
//!   "there should be one more CockroachDB node" or even "there should be six
//!   CockroachDB nodes" because two Nexus instances might _both_ decide to
//!   provision a new node and then we'd have too many.)  Plans must also be
//!   incremental enough that any execution of them should not break the system.
//!   For example, if between two consecutive blueprints the version of every
//!   CockroachDB node changed, then concurrent blueprint execution might try to
//!   update them all at once, bringing the whole Cockroach cluster down.  In
//!   this case, we need to use a sequence of blueprints that each only updates
//!   one node at a time to ensure that the system keeps working.
//!
//!   At any given time, the system has exactly one _target_ blueprint.  The
//!   deployment system is always attempting to make reality match this
//!   blueprint.  The system can be aware of more than one deployment blueprint,
//!   including past ones, later ones, those generated by Oxide support, etc.
//!
//! In terms of carrying it out, here's the basic idea:
//!
//! ```ignored
//! The Planner
//!
//!     fleet policy     (latest inventory)   (latest blueprint)
//!              \               |               /
//!               \              |              /
//!                +----------+  |  +----------/
//!                           |  |  |
//!                           v  v  v
//!
//!                          "planner"
//!               (eventually a background task)
//!                              |
//!                              v                 no
//!                     is a new blueprint necessary? ------> done
//!                              |
//!                              | yes
//!                              v
//!                     generate a new blueprint
//!                              |
//!                              |
//!                              v
//!                     commit blueprint to database
//!                              |
//!                              |
//!                              v
//!                            done
//!
//!
//! The Executor (better name?)
//!
//!     latest committed blueprint  latest inventory
//!                      |             |
//!                      |             |
//!                      +----+   +----+
//!                           |   |
//!                           v   v
//!
//!                         "executor"
//!                      (background task)
//!                             |
//!                             v
//!                     determine actions needed
//!                     take actions
//! ```
//!
//! The **planner** evaluates whether the current (target) blueprint is
//! consistent with the current policy.  If not, the task generates a new
//! blueprint that _is_ consistent with the current policy and attempts to make
//! that the new target.  (Multiple Nexus instances could try to do this
//! concurrently.  CockroachDB's strong consistency ensures that only one can
//! win.  The other Nexus instances must go back to evaluating the winning
//! blueprint before trying to change it again -- otherwise two Nexus instances
//! might fight over two equivalent blueprints.)
//!
//! An **execution** task periodically evaluates whether the state reflected in
//! the latest inventory collection is consistent with the current target
//! blueprint.  If not, it executes operations to bring reality into line with
//! the blueprint.  This means provisioning new zones, removing old zones,
//! adding instances to DNS, removing instances from DNS, carrying out firmware
//! updates, etc.

pub mod blueprint_builder;
pub mod example;
mod ip_allocator;
pub mod planner;
pub mod system;
