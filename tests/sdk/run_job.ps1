# One suite job in its own process, so its environment can't leak into the others.
# -Spec is a PowerShell script that sets $Kind, $Runner, $Test, $PairEnv, $RoleEnv,
# $Roles, $PeerParty, $Game, $Work, $Direct and $Session (a hashtable of environment variables).
param([Parameter(Mandatory)][string]$Spec)
$ErrorActionPreference = 'Continue'
. $Spec
foreach ($name in @($Session.Keys)) {
    Set-Item -Path "env:$name" -Value ([string]$Session[$name])
}
if ($Kind -eq 'photon') {
    & $Runner -PeerParty $PeerParty
    exit $LASTEXITCODE
}
& (Join-Path $PSScriptRoot 'run_pair.ps1') -Test $Test -Env @($PairEnv) -RoleEnv @($RoleEnv) -Roles @($Roles) -PeerParty $PeerParty -Game $Game -Work $Work -Direct:([bool]$Direct)
exit 0
