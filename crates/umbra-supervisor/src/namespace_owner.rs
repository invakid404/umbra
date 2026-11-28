//! Namespace ownership retained while a supervisor is split for finalization.
use umbra_overlay::NamespaceSession;

/// Ordinary ownership or registered-worker custody.
pub enum NamespaceOwner {
    /// Explicit ordinary namespace ownership.
    Unmanaged(Box<dyn NamespaceSession + Send>),
    /// Namespace whose destructor requires durable managed completion.
    #[cfg(target_os = "macos")]
    Managed(Box<crate::run_owner::RunOwner>),
}

impl NamespaceOwner {
    /// Whether durable custody, rather than ordinary finish, owns this namespace.
    pub fn is_managed(&self) -> bool {
        match self {
            Self::Unmanaged(_) => false,
            #[cfg(target_os = "macos")]
            Self::Managed(_) => true,
        }
    }
}

impl From<Box<dyn NamespaceSession + Send>> for NamespaceOwner {
    fn from(namespace: Box<dyn NamespaceSession + Send>) -> Self {
        Self::Unmanaged(namespace)
    }
}

impl std::ops::Deref for NamespaceOwner {
    type Target = dyn NamespaceSession;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Unmanaged(namespace) => namespace.as_ref(),
            #[cfg(target_os = "macos")]
            Self::Managed(owner) => owner.namespace_ref(),
        }
    }
}

impl std::ops::DerefMut for NamespaceOwner {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Unmanaged(namespace) => namespace.as_mut(),
            #[cfg(target_os = "macos")]
            Self::Managed(owner) => owner.namespace(),
        }
    }
}
