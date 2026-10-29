//! Turning a workload into something a target can run. Only Docker Compose for now;
//! Kubernetes manifests join in Phase 5. Everything here is pure: no I/O.

pub mod compose;
