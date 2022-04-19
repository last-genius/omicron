// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Database-friendly IPv6 addresses

use diesel::backend::Backend;
use diesel::backend::RawValue;
use diesel::deserialize;
use diesel::deserialize::FromSql;
use diesel::serialize;
use diesel::serialize::Output;
use diesel::serialize::ToSql;
use diesel::sql_types::Inet;
use ipnetwork::IpNetwork;
use ipnetwork::Ipv6Network;
use omicron_common::api::external::Error;

#[derive(
    Clone, Copy, AsExpression, FromSqlRow, PartialEq, Ord, PartialOrd, Eq,
)]
#[sql_type = "Inet"]
pub struct Ipv6Addr(std::net::Ipv6Addr);

NewtypeDebug! { () pub struct Ipv6Addr(std::net::Ipv6Addr); }
NewtypeFrom! { () pub struct Ipv6Addr(std::net::Ipv6Addr); }
NewtypeDeref! { () pub struct Ipv6Addr(std::net::Ipv6Addr); }

impl From<&std::net::Ipv6Addr> for Ipv6Addr {
    fn from(addr: &std::net::Ipv6Addr) -> Self {
        Self(*addr)
    }
}

impl<DB> ToSql<Inet, DB> for Ipv6Addr
where
    DB: Backend,
    IpNetwork: ToSql<Inet, DB>,
{
    fn to_sql<W: std::io::Write>(
        &self,
        out: &mut Output<W, DB>,
    ) -> serialize::Result {
        IpNetwork::V6(Ipv6Network::from(self.0)).to_sql(out)
    }
}

impl<DB> FromSql<Inet, DB> for Ipv6Addr
where
    DB: Backend,
    IpNetwork: FromSql<Inet, DB>,
{
    fn from_sql(bytes: RawValue<DB>) -> deserialize::Result<Self> {
        match IpNetwork::from_sql(bytes)?.ip() {
            std::net::IpAddr::V6(ip) => Ok(Self(ip)),
            v4 => {
                Err(Box::new(Error::internal_error(
                    format!("Expected an IPv6 address from the database, found IPv4: '{}'", v4).as_str()
                )))
            }
        }
    }
}
