//! Who a request speaks for, and the self-only rules that follow from it.
//!
//! Every transport (Signal today, the app API next) resolves its caller to a
//! `CallerScope` and then applies these rules, so a linked DM and an API
//! session can never disagree about what a member may touch.

use chotu_common::AppConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallerScope {
    LinkedDm { member_id: String },
    HouseholdGroup,
}

impl CallerScope {
    pub(crate) fn member_id(&self) -> Option<&str> {
        match self {
            Self::LinkedDm { member_id } => Some(member_id),
            Self::HouseholdGroup => None,
        }
    }

    pub(crate) fn allows_task_assignee(&self, assigned_to: Option<&str>) -> bool {
        match self {
            Self::LinkedDm { member_id } => {
                assigned_to.is_none_or(|assignee| member_id.eq_ignore_ascii_case(assignee))
            }
            Self::HouseholdGroup => true,
        }
    }
}

pub(crate) fn oauth_member_target(
    scope: &CallerScope,
    requested: &str,
) -> Result<String, &'static str> {
    match scope {
        CallerScope::HouseholdGroup => {
            Err("OAuth setup is only available in an authorized direct conversation.")
        }
        CallerScope::LinkedDm { member_id }
            if requested.trim().is_empty() || member_id.eq_ignore_ascii_case(requested.trim()) =>
        {
            Ok(member_id.clone())
        }
        CallerScope::LinkedDm { .. } => {
            Err("OAuth setup in a direct conversation can only target your own member account.")
        }
    }
}

pub(crate) fn resolve_task_target(
    scope: &CallerScope,
    requested: Option<String>,
    config: &AppConfig,
) -> Result<Option<String>, &'static str> {
    match scope {
        CallerScope::LinkedDm { member_id } => match requested {
            Some(target) if !scope.allows_task_assignee(Some(&target)) => {
                Err("Tasks in a direct conversation can only be added or assigned to you.")
            }
            _ => Ok(Some(member_id.clone())),
        },
        CallerScope::HouseholdGroup => Ok(requested.or_else(|| {
            config
                .family
                .members
                .first()
                .map(|member| member.id.clone())
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_dm_task_targets_are_self_only() {
        let mut config = AppConfig::default();
        config.family.members[0].signal_aci = Some("aci-alex".to_string());
        let dm = CallerScope::LinkedDm {
            member_id: "alex".to_string(),
        };
        let group = CallerScope::HouseholdGroup;

        assert_eq!(
            resolve_task_target(&dm, None, &config).unwrap().as_deref(),
            Some("alex")
        );
        assert_eq!(
            resolve_task_target(&dm, Some("ALEX".to_string()), &config)
                .unwrap()
                .as_deref(),
            Some("alex")
        );
        assert!(resolve_task_target(&dm, Some("jordan".to_string()), &config).is_err());
        assert!(dm.allows_task_assignee(None));
        assert!(dm.allows_task_assignee(Some("alex")));
        assert!(!dm.allows_task_assignee(Some("jordan")));
        assert!(group.allows_task_assignee(Some("jordan")));
        assert_eq!(
            resolve_task_target(&group, Some("jordan".to_string()), &config)
                .unwrap()
                .as_deref(),
            Some("jordan")
        );
    }

    #[test]
    fn oauth_target_policy_is_dm_self_only() {
        let dm = CallerScope::LinkedDm {
            member_id: "alex".to_string(),
        };
        assert_eq!(oauth_member_target(&dm, "").unwrap(), "alex");
        assert_eq!(oauth_member_target(&dm, "ALEX").unwrap(), "alex");
        assert!(oauth_member_target(&dm, "jordan").is_err());
        assert!(oauth_member_target(&CallerScope::HouseholdGroup, "alex").is_err());
    }
}
