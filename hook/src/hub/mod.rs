//! Where the SDK layers plug into the shared ones: static tables of what each layer
//! offers, so the shared code (DNS, the HTTPS server, invites, the overlay) loops over a
//! table instead of naming every SDK. Adding a layer means adding its entry here; the
//! entries themselves live with their layer.
//!
//! - `DNS` (net/dns.rs): whose hosts' lookups PeerParty answers, in the order tried.
//! - `HTTPS` (https/): the services on the local HTTPS server, in the order a request's
//!   Host is matched; the last takes what no other claims.
//! - `INVITES` (invite/): the SDKs whose lobbies and presence carry invites, in the
//!   order their presence is offered.
//! - `OTHER_LOBBIES` (invite/): lobbies that carry no invites but show where we are.
//! - `OVERLAY` (overlay.rs): who hears the overlay open and close.
//!
//! accounts.rs answers the one thing layers ask of each other: a player's ids on
//! another platform.

pub(crate) mod accounts;

use crate::https::HttpsService;
use crate::invite::InviteCarrier;
use crate::net::dns::DnsService;
use crate::overlay::OverlayListener;

pub static DNS: &[&DnsService] =
    &[&crate::playfab::DNS, &crate::ugs::DNS, &crate::xboxlive::DNS, &crate::photon::DNS, &crate::demonware::DNS, &crate::steam::web::DNS];

pub static HTTPS: &[&HttpsService] =
    &[&crate::ugs::HTTPS, &crate::demonware::HTTPS, &crate::xboxlive::HTTPS, &crate::steam::web::HTTPS, &crate::playfab::HTTPS];

pub static INVITES: &[&InviteCarrier] = &[&crate::steam::INVITES, &crate::eos::INVITES, &crate::playfab::mp::INVITES, &crate::galaxy::INVITES];

pub static OTHER_LOBBIES: &[&str] = &[crate::playfab::party::NS, crate::ugs::lobby::NS];

pub static OVERLAY: &[&OverlayListener] = &[&crate::steam::OVERLAY, &crate::eos::OVERLAY, &crate::galaxy::OVERLAY];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Front;
    use peerparty_protocol::catalog;

    #[test]
    fn the_tables_match_the_catalog() {
        // Every service on the HTTPS server is one the catalog says uses it, and back.
        let mut https: Vec<&str> = HTTPS.iter().map(|s| s.name).collect();
        let mut listed: Vec<&str> = catalog::https_layers().collect();
        https.sort_unstable();
        listed.sort_unstable();
        assert_eq!(https, listed);
        // Every DNS layer is a catalog layer.
        for d in DNS {
            assert!(catalog::LAYERS.iter().any(|l| l.id == d.layer), "{}", d.layer);
        }
        // The invite carriers are the fronts that carry invites, each once.
        let mut fronts: Vec<Front> = INVITES.iter().map(|c| c.front).collect();
        let mut carry: Vec<Front> = Front::ALL.into_iter().filter(|f| f.carries_invites()).collect();
        fronts.sort_by_key(|f| f.label());
        carry.sort_by_key(|f| f.label());
        assert_eq!(fronts, carry);
        assert_eq!(OVERLAY.len(), 3);
    }

    #[test]
    fn hosts_belong_to_one_dns_service() {
        for (host, layer) in [
            ("abc.playfabapi.com", "playfab"),
            ("lobby.services.api.unity.com", "ugs"),
            ("social.xboxlive.com", "xboxlive"),
            ("ns.exitgames.com", "photon"),
            ("title-pc-auth3.prod.demonware.net", "demonware"),
            ("api.steampowered.com", "steam"),
        ] {
            let owners: Vec<&str> = DNS.iter().filter(|d| (d.is_host)(host)).map(|d| d.layer).collect();
            assert_eq!(owners, [layer], "{host}");
        }
        assert!(DNS.iter().all(|d| !(d.is_host)("www.example.com")));
    }
}
