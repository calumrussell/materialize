// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Driver for the `mz sql` command.

use mz::command::sql::RunArgs;
use mz::context::Context;
use mz::error::Error;

use crate::mixin::ProfileArg;

#[derive(Debug, clap::Args)]
#[clap(trailing_var_arg = true)]
pub struct SqlCommand {
    #[clap(flatten)]
    profile: ProfileArg,
    #[clap(long, env = "MZ_CLUSTER")]
    /// Use the specified cluster
    cluster: Option<String>,
    /// Additional arguments to pass to `psql`.
    psql_args: Vec<String>,
}

pub async fn run(cx: Context, cmd: SqlCommand) -> Result<(), Error> {
    let mut cx = cx
        .activate_profile(cmd.profile.profile)?
        .activate_region()?;
    mz::command::sql::run(
        &mut cx,
        RunArgs {
            cluster: cmd.cluster,
            psql_args: cmd.psql_args,
        },
    )
    .await
}
