use mail_send::Credentials;

use super::{Item, Lookup, LookupResult};

impl Lookup {
    pub async fn contains(&self, entry: &str) -> Option<bool> {
        match self {
            Lookup::Remote(tx) => tx
                .lookup(Item::IsAccount(entry.to_string()))
                .await
                .map(|r| r.into()),
            Lookup::Sql(sql) => sql.exists(entry).await,
            Lookup::Local(entries) => Some(entries.contains(entry)),
        }
    }

    pub async fn lookup(&self, item: Item) -> Option<LookupResult> {
        match self {
            Lookup::Remote(tx) => tx.lookup(item).await,

            Lookup::Sql(sql) => match item {
                Item::IsAccount(account) => sql.exists(&account).await.map(LookupResult::from),
                Item::Authenticate(credentials) => match credentials {
                    Credentials::Plain { username, secret }
                    | Credentials::XOauth2 { username, secret } => sql
                        .fetch_one(&username)
                        .await
                        .map(|pwd| LookupResult::from(pwd.map_or(false, |pwd| pwd == secret))),
                    Credentials::OAuthBearer { token } => {
                        sql.exists(&token).await.map(LookupResult::from)
                    }
                },
                Item::Verify(account) => sql.fetch_many(&account).await.map(LookupResult::from),
                Item::Expand(list) => sql.fetch_many(&list).await.map(LookupResult::from),
            },

            Lookup::Local(list) => match item {
                Item::IsAccount(item) => Some(list.contains(&item).into()),
                Item::Verify(_item) | Item::Expand(_item) => {
                    #[cfg(test)]
                    for list_item in list {
                        if let Some((prefix, suffix)) = list_item.split_once(':') {
                            if prefix == _item {
                                return Some(LookupResult::Values(
                                    suffix.split(',').map(|i| i.to_string()).collect::<Vec<_>>(),
                                ));
                            }
                        }
                    }
                    Some(LookupResult::False)
                }
                Item::Authenticate(credentials) => {
                    let entry = match credentials {
                        Credentials::Plain { username, secret }
                        | Credentials::XOauth2 { username, secret } => {
                            format!("{username}:{secret}")
                        }
                        Credentials::OAuthBearer { token } => token,
                    };

                    Some(list.contains(&entry).into())
                }
            },
        }
    }
}
