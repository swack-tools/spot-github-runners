//! Read endpoints that expose fleet or admin state (`/api/overview`, `GET /api/scaling`)
//! must refuse anonymous and non-admin callers.

use spot_dashboard_edge::auth::*;

fn session(sub: &str, uid: u64) -> SessionData {
    SessionData {
        exp: 2_000_000_000,
        iat: 1_900_000_000,
        sub: sub.to_string(),
        tgt: Vec::new(),
        uid,
        v: 1,
    }
}

#[test]
fn anonymous_caller_is_unauthenticated() {
    assert_eq!(
        admin_read_access(None, &[111_111]),
        AdminReadAccess::Unauthenticated
    );
}

#[test]
fn owner_is_allowed_without_being_added() {
    let owner = session(ALLOWED_ADMIN_LOGIN, ALLOWED_ADMIN_ID);
    assert_eq!(
        admin_read_access(Some(&owner), &[]),
        AdminReadAccess::Allowed
    );
}

#[test]
fn added_admin_is_allowed() {
    let admin = session("added_user", 111_111);
    assert_eq!(
        admin_read_access(Some(&admin), &[111_111]),
        AdminReadAccess::Allowed
    );
}

#[test]
fn signed_in_stranger_is_forbidden() {
    let stranger = session("someone_else", 333_333);
    assert_eq!(
        admin_read_access(Some(&stranger), &[111_111]),
        AdminReadAccess::Forbidden
    );
}

#[test]
fn owner_login_with_wrong_id_is_forbidden() {
    let spoof = session(ALLOWED_ADMIN_LOGIN, 333_333);
    assert_eq!(
        admin_read_access(Some(&spoof), &[111_111]),
        AdminReadAccess::Forbidden
    );
}
