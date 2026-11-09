// Stage 4 interop probe: clinfer's REAL station search against live origins.
//
// Runs `Dicomweb::search_workitems_for_station` - the Stage 2 code itself, not
// a re-implementation - against dcm4chee-arc and against a Clarus build, so the
// wire spelling the client sends is exercised end to end.
//
// Both origins must be reachable; the addresses are the ones the interop gate
// uses (see the sprint report for the commands that boot and seed them).

use clinfer::dicomweb::Dicomweb;
use clinfer::http::Client;

fn main() {
    let targets = [
        (
            "dcm4chee",
            "http://127.0.0.1:8080/dcm4chee-arc/aets/DCM4CHEE/rs",
            "STACODE1",
        ),
        (
            "dcm4chee",
            "http://127.0.0.1:8080/dcm4chee-arc/aets/DCM4CHEE/rs",
            "STACODE2",
        ),
        ("clarus", "http://127.0.0.1:8027/dicomweb", "PROBE_A"),
        ("clarus", "http://127.0.0.1:8027/dicomweb", "PROBE_B"),
        (
            "clarus",
            "http://127.0.0.1:8027/dicomweb",
            "NO_SUCH_STATION",
        ),
    ];

    for (name, base, station) in targets {
        let http = match Client::new(base, "probe") {
            Ok(c) => c,
            Err(e) => {
                println!("{name:9} station={station:16} CLIENT ERROR {e:?}");
                continue;
            }
        };
        let client = Dicomweb::new(http);
        match client.search_workitems_for_station(station) {
            Ok(uids) => println!(
                "{name:9} station={station:16} OK hits={} {uids:?}",
                uids.len()
            ),
            Err(e) => println!("{name:9} station={station:16} ERR {e:?}"),
        }
    }
}
