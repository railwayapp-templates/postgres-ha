#!/bin/bash
# Copy and paste these commands ONE AT A TIME into your terminal
# The Railway CLI will prompt you to create services interactively

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Copy/Paste Deployment Commands"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Run these commands ONE AT A TIME:"
echo ""

cat << 'EOF'

# 1. Deploy etcd-1
cd /Users/paulocabral/software/railway/mono/templates/postgres-ha/etcd-1
railway up
# When prompted: Create new service, name it "etcd-1"

# 2. Deploy etcd-2
cd /Users/paulocabral/software/railway/mono/templates/postgres-ha/etcd-2
railway up
# When prompted: Create new service, name it "etcd-2"

# 3. Deploy etcd-3
cd /Users/paulocabral/software/railway/mono/templates/postgres-ha/etcd-3
railway up
# When prompted: Create new service, name it "etcd-3"

# 4. Deploy postgres-1
cd /Users/paulocabral/software/railway/mono/templates/postgres-ha/postgres-patroni
railway up
# When prompted: Create new service, name it "postgres-1"

# 5. Deploy postgres-2 (same directory!)
railway up
# When prompted: Create new service, name it "postgres-2"

# 6. Deploy postgres-3 (same directory!)
railway up
# When prompted: Create new service, name it "postgres-3"

# 7. Deploy pgpool
cd /Users/paulocabral/software/railway/mono/templates/postgres-ha/pgpool
railway up
# When prompted: Create new service, name it "pgpool"

# Done! Now configure variables in Railway dashboard.

EOF
